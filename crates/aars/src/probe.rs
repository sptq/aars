//! Odpytywanie hosta o sprzęt: tylko listy, nic nie jest otwierane.

use aars_core::hardware::probe::{GpioChipInfo, HardwareProbe};

pub struct HostProbe;

impl HardwareProbe for HostProbe {
    fn audio_outputs(&self) -> Result<Vec<String>, String> {
        aars_audio::output::output_device_names().map_err(|e| e.to_string())
    }

    fn serial_ports(&self) -> Result<Vec<String>, String> {
        aars_ptt::probe::serial_ports()
    }

    fn gpio_chips(&self) -> Result<Vec<GpioChipInfo>, String> {
        aars_ptt::probe::gpio_chips()
    }
}
