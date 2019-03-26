use crate::{Result, UsbDirection};
use crate::bus::{UsbBusAllocator, UsbBus, PollResult};
use crate::class::{UsbClass, ControlIn, ControlOut};
use crate::control;
use crate::control_pipe::ControlPipe;
use crate::descriptor::descriptor_type;
use crate::endpoint::{EndpointType, EndpointAddress};
use core::marker::PhantomData;


/// A trait implemented by generated USB configuration.
pub trait CustomStringDescriptorProvider<B: UsbBus> {
    /// Writes out custom string descriptor
    fn get_custom_string_descriptor(id: usize, xfer: ControlIn<B>) -> Result<()> {
        let _ = id;
        xfer.reject()
    }
}

/// A trait implemented by generated USB configuration.
pub trait DescriptorProvider<B: UsbBus>: CustomStringDescriptorProvider<B> {
    /// Writes out device descriptor blob
    fn get_device_descriptor(buffer: &mut [u8]) -> Result<usize>;

    /// Writes out configuration descriptor blob
    fn get_configuration_descriptor(buffer: &mut [u8]) -> Result<usize>;

    /// Writes out string descriptor
    ///
    /// This function will reject transfer if requested string descriptor isn't defined
    fn get_string_descriptor(lang_id: u16, index: u8, xfer: ControlIn<B>) -> Result<()>;

    /// Returns endpoint 0 maximum packet size
    fn get_ep0_max_packet_size() -> u8;

    /// Writes requested descriptor
    fn get_descriptor(xfer: ControlIn<B>) -> Result<()> {
        let req = *xfer.request();

        let (dtype, index) = req.descriptor_type_index();

        match dtype {
            descriptor_type::DEVICE => xfer.accept(|buf| {
                Self::get_device_descriptor(buf)
            }),
            descriptor_type::CONFIGURATION => xfer.accept(|buf| {
                Self::get_configuration_descriptor(buf)
            }),
            descriptor_type::STRING => Self::get_string_descriptor(req.index, index, xfer),
            _ => xfer.reject(),
        }
    }
}

/// The global state of the USB device.
///
/// In general class traffic is only possible in the `Configured` state.
#[repr(u8)]
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum UsbDeviceState {
    /// The USB device has just been created or reset.
    Default,

    /// The USB device has received an address from the host.
    Addressed,

    /// The USB device has been configured and is fully functional.
    Configured,

    /// The USB device has been suspended by the host or it has been unplugged from the USB bus.
    Suspend,
}

// Maximum number of endpoints in one direction. Specified by the USB specification.
const MAX_ENDPOINTS: usize = 16;

/// A USB device consisting of one or more device classes.
pub struct UsbDevice<'a, B: UsbBus, P: DescriptorProvider<B>> {
    bus: &'a B,
    control: ControlPipe<'a, B>,
    device_state: UsbDeviceState,
    remote_wakeup_enabled: bool,
    self_powered: bool,
    pending_address: u8,
    _provider: PhantomData<P>,
}

pub(crate) struct Config<'a> {
    pub device_class: u8,
    pub device_sub_class: u8,
    pub device_protocol: u8,
    pub max_packet_size_0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_release: u16,
    pub manufacturer: Option<&'a str>,
    pub product: Option<&'a str>,
    pub serial_number: Option<&'a str>,
    pub self_powered: bool,
    pub supports_remote_wakeup: bool,
    pub max_power: u8,
}

/// The bConfiguration value for the single configuration supported by this device.
pub const CONFIGURATION_VALUE: u8 = 1;

/// The default value for bAlternateSetting for all interfaces.
pub const DEFAULT_ALTERNATE_SETTING: u8 = 0;

type ClassList<'a, B> = [&'a mut dyn UsbClass<B>];

impl<B: UsbBus, P: DescriptorProvider<B>> UsbDevice<'_, B, P> {
    pub fn new<'a>(alloc: &'a UsbBusAllocator<B>) -> UsbDevice<'a, B, P>
    {
        let control_out = alloc.alloc(Some(0.into()), EndpointType::Control,
            P::get_ep0_max_packet_size as u16, 0).expect("failed to alloc control endpoint");

        let control_in = alloc.alloc(Some(0.into()), EndpointType::Control,
            P::get_ep0_max_packet_size as u16, 0).expect("failed to alloc control endpoint");

        let bus = alloc.freeze();

        UsbDevice {
            bus,
            control: ControlPipe::new(control_out, control_in),
            device_state: UsbDeviceState::Default,
            remote_wakeup_enabled: false,
            self_powered: false,
            pending_address: 0,
            _provider: PhantomData
        }
    }

    /// Gets the current state of the device.
    ///
    /// In general class traffic is only possible in the `Configured` state.
    pub fn state(&self) -> UsbDeviceState {
        self.device_state
    }

    /// Gets whether host remote wakeup has been enabled by the host.
    pub fn remote_wakeup_enabled(&self) -> bool {
        self.remote_wakeup_enabled
    }

    /// Gets whether the device is currently self powered.
    pub fn self_powered(&self) -> bool {
        self.self_powered
    }

    /// Sets whether the device is currently self powered.
    pub fn set_self_powered(&mut self, is_self_powered: bool) {
        self.self_powered = is_self_powered;
    }

    /// Forces a reset on the UsbBus.
    pub fn force_reset(&mut self) -> Result<()> {
        self.bus.force_reset()
    }

    /// Polls the [`UsbBus`] for new events and dispatches them to the provided classes. Returns
    /// true if one of the classes may have data available for reading or be ready for writing,
    /// false otherwise. This should be called periodically as often as possible for the best data
    /// rate, or preferably from an interrupt handler. Must be called at least one every 10
    /// milliseconds while connected to the USB host to be USB compliant.
    ///
    /// Note: The list of classes passed in must be the same for every call while the device is
    /// configured, or the device may enumerate incorrectly or otherwise misbehave. The easiest way
    /// to do this is to call the `poll` method in only one place in your code, as follows:
    ///
    /// ``` ignore
    /// usb_dev.poll(&mut [&mut class1, &mut class2]);
    /// ```
    ///
    /// Strictly speaking the list of classes is allowed to change between polls if the device has
    /// been reset, which is indicated by `state` being equal to [`UsbDeviceState::Default`], but
    /// this is likely to cause compatibility problems with some operating systems.
    pub fn poll(&mut self, classes: &mut ClassList<'_, B>) -> bool {
        let pr = self.bus.poll();

        if self.device_state == UsbDeviceState::Suspend {
            match pr {
                PollResult::Suspend | PollResult::None => { return false; },
                _ => {
                    self.bus.resume();
                    self.device_state = UsbDeviceState::Default;
                },
            }
        }

        match pr {
            PollResult::None => { }
            PollResult::Reset => self.reset(classes),
            PollResult::Data { ep_out, ep_in_complete, ep_setup } => {
                // Combine bit fields for quick tests
                let mut eps = ep_out | ep_in_complete | ep_setup;

                // Pending events for endpoint 0?
                if (eps & 1) != 0 {
                    let xfer = if (ep_setup & 1) != 0 {
                        self.control.handle_setup()
                    } else if (ep_out & 1) != 0 {
                        self.control.handle_out()
                    } else {
                        None
                    };

                    match xfer {
                        Some(UsbDirection::In) => self.control_in(classes),
                        Some(UsbDirection::Out) => self.control_out(classes),
                        _ => (),
                    };

                    if (ep_in_complete & 1) != 0 {
                        let completed = self.control.handle_in_complete();

                        if completed && self.pending_address != 0 {
                            self.bus.set_device_address(self.pending_address);
                            self.pending_address = 0;

                            self.device_state = UsbDeviceState::Addressed;
                        }
                    }

                    eps &= !1;
                }

                // Pending events for other endpoints?
                if eps != 0 {
                    let mut bit = 2u16;

                    for i in 1..MAX_ENDPOINTS {
                        if (ep_setup & bit) != 0 {
                            for cls in classes.iter_mut() {
                                cls.endpoint_setup(
                                    EndpointAddress::from_parts(i, UsbDirection::Out));
                            }
                        } else if (ep_out & bit) != 0 {
                            for cls in classes.iter_mut() {
                                cls.endpoint_out(
                                    EndpointAddress::from_parts(i, UsbDirection::Out));
                            }
                        }

                        if (ep_in_complete & bit) != 0 {
                            for cls in classes.iter_mut() {
                                cls.endpoint_in_complete(
                                    EndpointAddress::from_parts(i, UsbDirection::In));
                            }
                        }

                        eps &= !bit;

                        if eps == 0 {
                            // No more pending events for higher endpoints
                            break;
                        }

                        bit <<= 1;
                    }
                }

                for cls in classes.iter_mut() {
                    cls.poll();
                }

                return true;
            },
            PollResult::Resume => { }
            PollResult::Suspend => {
                self.bus.suspend();
                self.device_state = UsbDeviceState::Suspend;
            }
        }

        return false;
    }

    fn control_in(&mut self, classes: &mut ClassList<'_, B>) {
        use crate::control::{Request, Recipient};

        let req = *self.control.request();
        let mut ctrl = Some(&mut self.control);

        for cls in classes.iter_mut() {
            cls.control_in(ControlIn::new(&mut ctrl));

            if ctrl.is_none() {
                return;
            }
        }

        if req.request_type == control::RequestType::Standard {
            let xfer = ControlIn::new(&mut ctrl);

            match (req.recipient, req.request) {
                (Recipient::Device, Request::GET_STATUS) => {
                    let status: u16 = 0x0000
                        | if self.self_powered { 0x0001 } else { 0x0000 }
                        | if self.remote_wakeup_enabled { 0x0002 } else { 0x0000 };

                    xfer.accept_with(&status.to_le_bytes()).ok();
                },

                (Recipient::Interface, Request::GET_STATUS) => {
                    let status: u16 = 0x0000;

                    xfer.accept_with(&status.to_le_bytes()).ok();
                },

                (Recipient::Endpoint, Request::GET_STATUS) => {
                    let ep_addr = ((req.index as u8) & 0x8f).into();

                    let status: u16 = 0x0000
                        | if self.bus.is_stalled(ep_addr) { 0x0001 } else { 0x0000 };

                    xfer.accept_with(&status.to_le_bytes()).ok();
                },

                (Recipient::Device, Request::GET_DESCRIPTOR) => {
                    P::get_descriptor(xfer).ok();
                },

                (Recipient::Device, Request::GET_CONFIGURATION) => {
                    xfer.accept_with(&CONFIGURATION_VALUE.to_le_bytes()).ok();
                },

                (Recipient::Interface, Request::GET_INTERFACE) => {
                    // TODO: change when alternate settings are implemented
                    xfer.accept_with(&DEFAULT_ALTERNATE_SETTING.to_le_bytes()).ok();
                },

                _ => (),
            };
        }

        if let Some(ctrl) = ctrl {
            ctrl.reject().ok();
        }
    }

    fn control_out(&mut self, classes: &mut ClassList<'_, B>) {
        use crate::control::{Request, Recipient};

        let req = *self.control.request();
        let mut ctrl = Some(&mut self.control);

        for cls in classes {
            cls.control_out(ControlOut::new(&mut ctrl));

            if ctrl.is_none() {
                return;
            }
        }

        if req.request_type == control::RequestType::Standard {
            let xfer = ControlOut::new(&mut ctrl);

            const CONFIGURATION_VALUE_U16: u16 = CONFIGURATION_VALUE as u16;
            const DEFAULT_ALTERNATE_SETTING_U16: u16 = DEFAULT_ALTERNATE_SETTING as u16;

            match (req.recipient, req.request, req.value) {
                (Recipient::Device, Request::CLEAR_FEATURE, Request::FEATURE_DEVICE_REMOTE_WAKEUP) => {
                    self.remote_wakeup_enabled = false;
                    xfer.accept().ok();
                },

                (Recipient::Endpoint, Request::CLEAR_FEATURE, Request::FEATURE_ENDPOINT_HALT) => {
                    self.bus.set_stalled(((req.index as u8) & 0x8f).into(), false);
                    xfer.accept().ok();
                },

                (Recipient::Device, Request::SET_FEATURE, Request::FEATURE_DEVICE_REMOTE_WAKEUP) => {
                    self.remote_wakeup_enabled = true;
                    xfer.accept().ok();
                },

                (Recipient::Endpoint, Request::SET_FEATURE, Request::FEATURE_ENDPOINT_HALT) => {
                    self.bus.set_stalled(((req.index as u8) & 0x8f).into(), true);
                    xfer.accept().ok();
                },

                (Recipient::Device, Request::SET_ADDRESS, 1..=127) => {
                    self.pending_address = req.value as u8;
                    xfer.accept().ok();
                },

                (Recipient::Device, Request::SET_CONFIGURATION, CONFIGURATION_VALUE_U16) => {
                    self.device_state = UsbDeviceState::Configured;
                    xfer.accept().ok();
                },

                (Recipient::Interface, Request::SET_INTERFACE, DEFAULT_ALTERNATE_SETTING_U16) => {
                    // TODO: do something when alternate settings are implemented
                    xfer.accept().ok();
                },

                _ => { xfer.reject().ok(); return; },
            }
        }

        if let Some(ctrl) = ctrl {
            ctrl.reject().ok();
        }
    }

    fn reset(&mut self, classes: &mut ClassList<'_, B>) {
        self.bus.reset();

        self.device_state = UsbDeviceState::Default;
        self.remote_wakeup_enabled = false;
        self.pending_address = 0;

        self.control.reset();

        for cls in classes {
            cls.reset();
        }
    }
}
