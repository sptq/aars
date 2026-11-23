//! Lista tego, co host ma do sterowania PTT. Tylko listowanie: żaden port
//! nie jest otwierany (otwarcie tty podnosi DTR/RTS i może zakluczować radio),
//! a żadna linia GPIO nie jest żądana.

use aars_core::hardware::probe::GpioChipInfo;

pub fn serial_ports() -> Result<Vec<String>, String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map_err(|e| format!("lista portów szeregowych: {e}"))?
        .into_iter()
        .map(|p| p.port_name)
        .collect();
    ports.sort();
    Ok(ports)
}

#[cfg(target_os = "linux")]
pub fn gpio_chips() -> Result<Vec<GpioChipInfo>, String> {
    let paths = gpiocdev::chip::chips().map_err(|e| format!("lista układów GPIO: {e}"))?;
    let mut out = Vec::new();
    for path in paths {
        // Układ, którego nie da się odczytać, jest po prostu pomijany.
        let Ok(info) = gpiocdev::Chip::from_path(&path).and_then(|c| c.info()) else {
            continue;
        };
        out.push(GpioChipInfo {
            path: path.to_string_lossy().into_owned(),
            label: info.label,
            lines: info.num_lines,
        });
    }
    Ok(out)
}

#[cfg(not(target_os = "linux"))]
pub fn gpio_chips() -> Result<Vec<GpioChipInfo>, String> {
    Err("GPIO wymaga Linuksa z /dev/gpiochip*".to_owned())
}
