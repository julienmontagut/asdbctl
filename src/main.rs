use clap::{arg, Command};
use log::*;
use std::{
    error::Error,
    fs::{self, File},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    vec::Vec,
};

const REPORT_ID: u8 = 1;

const MIN_BRIGHTNESS: u32 = 400;
const MAX_BRIGHTNESS: u32 = 60000;
const BRIGHTNESS_RANGE: u32 = MAX_BRIGHTNESS - MIN_BRIGHTNESS;

const SD_VENDOR_ID: u32 = 0x05ac;
const SD_INTERFACE_NR: u8 = 0x7;
const SD_PRODUCT_IDS: [u32; 3] = [
    0x1114, // Studio Display (2022)
    0x1116, // Studio Display XDR (2026)
    0x1118, // Studio Display (2026)
];

// HIDIOCSFEATURE and HIDIOCGFEATURE from linux/hidraw.h
const fn hidioc(nr: u32, len: usize) -> libc::Ioctl {
    ((3 << 30) | ((len as u32) << 16) | ((b'H' as u32) << 8) | nr) as libc::Ioctl
}

#[derive(Debug, PartialEq)]
struct Display {
    devnode: PathBuf,
    serial: String,
}

fn get_brightness(handle: &File) -> Result<u32, Box<dyn Error>> {
    let mut buf = [0_u8; 7]; // report id, 4 bytes brightness, 2 bytes unknown
    buf[0] = REPORT_ID;
    let size = unsafe {
        libc::ioctl(
            handle.as_raw_fd(),
            hidioc(0x07, buf.len()),
            buf.as_mut_ptr(),
        )
    };
    if size < 0 {
        Err(io::Error::last_os_error())?
    }
    if size as usize != buf.len() {
        Err(format!(
            "Get HID feature report: Expected a size of {}, got {}",
            buf.len(),
            size
        ))?
    }
    let brightness = u32::from_le_bytes(buf[1..5].try_into()?);
    Ok(brightness)
}

fn get_brightness_percent(handle: &File) -> Result<u8, Box<dyn Error>> {
    let value = (get_brightness(handle)? - MIN_BRIGHTNESS) as f32;
    let value_percent = (value / BRIGHTNESS_RANGE as f32 * 100.0) as u8;
    Ok(value_percent)
}

fn set_brightness(handle: &File, brightness: u32) -> Result<(), Box<dyn Error>> {
    let mut buf = [0_u8; 7]; // report id, 4 bytes brightness, 2 bytes unknown
    buf[0] = REPORT_ID;
    buf[1..5].copy_from_slice(&brightness.to_le_bytes());
    let size = unsafe { libc::ioctl(handle.as_raw_fd(), hidioc(0x06, buf.len()), buf.as_ptr()) };
    if size < 0 {
        Err(io::Error::last_os_error())?
    }
    Ok(())
}

fn set_brightness_percent(handle: &File, brightness: u8) -> Result<(), Box<dyn Error>> {
    let nits =
        ((brightness as f32 * BRIGHTNESS_RANGE as f32) / 100.0 + MIN_BRIGHTNESS as f32) as u32;
    let nits = std::cmp::min(nits, MAX_BRIGHTNESS);
    let nits = std::cmp::max(nits, MIN_BRIGHTNESS);
    set_brightness(handle, nits)?;
    Ok(())
}

fn studio_displays(sysfs: &Path) -> Result<Vec<Display>, Box<dyn Error>> {
    let class = sysfs.join("class/hidraw");
    if !class.exists() {
        return Ok(Vec::new());
    }
    let mut displays = Vec::new();
    for entry in fs::read_dir(class)? {
        let entry = entry?;
        let device = entry.path().join("device");
        let uevent = fs::read_to_string(device.join("uevent"))?;
        let mut vendor_id = 0;
        let mut product_id = 0;
        let mut serial = String::new();
        for line in uevent.lines() {
            if let Some(hid_id) = line.strip_prefix("HID_ID=") {
                // bus:vendor:product, e.g. 0003:000005AC:00001114
                let mut ids = hid_id.split(':').skip(1);
                vendor_id = u32::from_str_radix(ids.next().unwrap_or_default(), 16)?;
                product_id = u32::from_str_radix(ids.next().unwrap_or_default(), 16)?;
            } else if let Some(uniq) = line.strip_prefix("HID_UNIQ=") {
                serial = uniq.to_string();
            }
        }
        if vendor_id != SD_VENDOR_ID || !SD_PRODUCT_IDS.contains(&product_id) {
            continue;
        }
        // The parent of the HID device is its USB interface
        let interface = fs::read_to_string(device.join("../bInterfaceNumber"))?;
        if u8::from_str_radix(interface.trim(), 16)? != SD_INTERFACE_NR {
            continue;
        }
        displays.push(Display {
            devnode: Path::new("/dev").join(entry.file_name()),
            serial,
        });
    }
    displays.sort_by(|a, b| a.devnode.cmp(&b.devnode));
    Ok(displays)
}

fn cli() -> Command {
    Command::new("asdbctl")
        .about("Tool to get or set the brightness for Apple Studio Displays")
        .subcommand_required(true)
        .arg(
            arg!(-s --serial <SERIAL> "Serial number of the display for which to adjust the brightness")
        )
        .arg(
            arg!(-v --verbose ... "Turn debugging information on")
        )
        .subcommand(Command::new("get").about("Get the current brightness in %"))
        .subcommand(
            Command::new("set")
                .about("Set the current brightness in %")
                .arg(
                    arg!(<BRIGHTNESS> "The remote to target")
                        .value_parser(clap::value_parser!(u8).range(0..101)),
                )
                .arg_required_else_help(true),
        )
        .subcommand(
            Command::new("up")
                .arg(
                    arg!(-s --step <STEP> "Step size in percent")
                        .required(false)
                        .default_value("10")
                        .value_parser(clap::value_parser!(u8).range(1..101)),
                )
                .about("Increase the brightness"),
        )
        .subcommand(
            Command::new("down")
                .arg(
                    arg!(-s --step <STEP> "Step size in percent")
                        .required(false)
                        .default_value("10")
                        .value_parser(clap::value_parser!(u8).range(1..101)),
                )
                .about("Decrease the brightness"),
        )
}

fn main() -> Result<(), Box<dyn Error>> {
    let matches = cli().get_matches();
    let verbosity = *matches
        .get_one::<u8>("verbose")
        .expect("Counts are defaulted");
    env_logger::Builder::new()
        .filter_level(match verbosity {
            0 => LevelFilter::Error,
            1 => LevelFilter::Warn,
            2 => LevelFilter::Info,
            3 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        })
        .init();

    let displays = studio_displays(Path::new("/sys"))?;
    if displays.is_empty() {
        Err("No Apple Studio Display found")?;
    }

    for display in displays {
        let handle = File::options()
            .read(true)
            .write(true)
            .open(&display.devnode)
            .map_err(|e| format!("Failed to open {}: {}", display.devnode.display(), e))?;
        info!("display serial number {}", display.serial);
        if let Some(serial) = matches.get_one::<String>("serial") {
            if display.serial != *serial {
                continue;
            }
        }
        match matches.subcommand() {
            Some(("get", _)) => {
                let brightness = get_brightness_percent(&handle)?;
                println!("brightness {}", brightness);
            }
            Some(("set", sub_matches)) => {
                let brightness = *sub_matches.get_one::<u8>("BRIGHTNESS").expect("required");
                set_brightness_percent(&handle, brightness)?;
            }
            Some(("up", sub_matches)) => {
                let step = *sub_matches.get_one::<u8>("step").expect("required");
                let brightness = get_brightness_percent(&handle)?;
                let new_brightness = std::cmp::min(100, brightness + step);
                set_brightness_percent(&handle, new_brightness)?;
            }
            Some(("down", sub_matches)) => {
                let step = *sub_matches.get_one::<u8>("step").expect("required");
                let brightness = get_brightness_percent(&handle)?;
                let new_brightness = std::cmp::max(0, brightness as i32 - step as i32) as u8;
                set_brightness_percent(&handle, new_brightness)?;
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fake_sysfs(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("asdbctl-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("class/hidraw")).unwrap();
        root
    }

    fn add_hidraw(root: &Path, hidraw: &str, hid_id: &str, interface: Option<&str>, uniq: &str) {
        let parent = root.join("devices").join(hidraw);
        let device = parent.join(hid_id);
        fs::create_dir_all(&device).unwrap();
        fs::write(
            device.join("uevent"),
            format!("DRIVER=hid-generic\nHID_ID={hid_id}\nHID_UNIQ={uniq}\n"),
        )
        .unwrap();
        if let Some(interface) = interface {
            fs::write(parent.join("bInterfaceNumber"), format!("{interface}\n")).unwrap();
        }
        let class = root.join("class/hidraw").join(hidraw);
        fs::create_dir_all(&class).unwrap();
        symlink(&device, class.join("device")).unwrap();
    }

    #[test]
    fn finds_only_the_brightness_interface_of_an_attached_studio_display() {
        let root = fake_sysfs("brightness-interface");
        add_hidraw(&root, "hidraw0", "0018:00000488:0000104A", None, "");
        add_hidraw(&root, "hidraw10", "0003:000019F5:00003245", Some("02"), "");
        add_hidraw(
            &root,
            "hidraw12",
            "0003:000005AC:00001114",
            Some("05"),
            "00008030-0001348E3C90802E",
        );
        add_hidraw(
            &root,
            "hidraw13",
            "0003:000005AC:00001114",
            Some("06"),
            "00008030-0001348E3C90802E",
        );
        add_hidraw(
            &root,
            "hidraw14",
            "0003:000005AC:00001114",
            Some("07"),
            "00008030-0001348E3C90802E",
        );

        let displays = studio_displays(&root).unwrap();

        assert_eq!(
            displays,
            vec![Display {
                devnode: PathBuf::from("/dev/hidraw14"),
                serial: "00008030-0001348E3C90802E".to_string(),
            }]
        );
    }

    #[test]
    fn finds_every_supported_studio_display_model_and_ignores_other_apple_devices() {
        let root = fake_sysfs("models");
        add_hidraw(
            &root,
            "hidraw3",
            "0003:000005AC:00001116",
            Some("07"),
            "XDR-SERIAL",
        );
        add_hidraw(&root, "hidraw4", "0003:000005AC:00001118", Some("07"), "");
        add_hidraw(
            &root,
            "hidraw5",
            "0003:000005AC:00001115",
            Some("07"),
            "OTHER-APPLE",
        );

        let displays = studio_displays(&root).unwrap();

        assert_eq!(
            displays,
            vec![
                Display {
                    devnode: PathBuf::from("/dev/hidraw3"),
                    serial: "XDR-SERIAL".to_string(),
                },
                Display {
                    devnode: PathBuf::from("/dev/hidraw4"),
                    serial: String::new(),
                },
            ]
        );
    }

    #[test]
    fn finds_no_display_when_no_hidraw_device_exists() {
        let root = std::env::temp_dir().join(format!("asdbctl-{}-no-hidraw", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let displays = studio_displays(&root).unwrap();

        assert_eq!(displays, vec![]);
    }
}
