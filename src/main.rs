use std::io::{self, ErrorKind::TimedOut, Read, Result};
use std::str::from_utf8;
use std::thread;
use std::time::{Duration, Instant};
use std::{fs::File, io::BufReader};

use async_io::{block_on, Timer};
use clap::{Parser, Subcommand};
use futures_lite::FutureExt;
use nusb::{
    transfer::{ControlIn, ControlOut, ControlType, Direction, Recipient, RequestBuffer},
    Device, Interface, Speed,
};

const KENDRYTE_VID: u16 = 0x29f1;
const K230D_PID: u16 = 0x0230;

const CLAIM_INTERFACE_TIMEOUT: Duration = Duration::from_secs(1);
const CLAIM_INTERFACE_PERIOD: Duration = Duration::from_micros(200);

fn claim_interface(d: &Device, ii: u8) -> std::result::Result<Interface, String> {
    let now = Instant::now();
    while Instant::now() <= now + CLAIM_INTERFACE_TIMEOUT {
        match d.claim_interface(ii) {
            Ok(i) => {
                return Ok(i);
            }
            Err(_) => {
                thread::sleep(CLAIM_INTERFACE_PERIOD);
            }
        }
    }
    Err("failure claiming USB interface".into())
}

const EP0_GET_CPU_INFO: u8 = 0x0;
const EP0_SET_DATA_ADDRESS: u8 = 0x1;
const EP0_SET_DATA_LENGTH: u8 = 0x2;
const EP0_FLUSH_CACHES: u8 = 0x3;
const EP0_PROG_START: u8 = 0x4;

const SRAM_RUN_BASE: &str = "0x80360000";
const MASK_ROM_BASE: usize = 0x9120_0000;

const CHUNK_SIZE: usize = 512;

#[derive(Debug, Subcommand)]
enum Command {
    /// Print CPU info
    #[clap(verbatim_doc_comment)]
    CpuInfo,
    /// Jump back to mask ROM
    #[clap(verbatim_doc_comment)]
    Rom,
    /// Load binary from file to memory
    #[clap(verbatim_doc_comment)]
    Load {
        #[clap(long, short, value_parser=clap_num::maybe_hex::<u32>, default_value = SRAM_RUN_BASE)]
        address: u32,
        file_name: String,
    },
    /// Run binary code from file
    #[clap(verbatim_doc_comment)]
    Run {
        #[clap(long, short, value_parser=clap_num::maybe_hex::<u32>, default_value = SRAM_RUN_BASE)]
        address: u32,
        file_name: String,
    },
}

/// Kendryte mask ROM loader tool
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Command to run
    #[command(subcommand)]
    cmd: Command,
}

fn cmd_in(i: &Interface, buf: &mut [u8], request: u8, val: u32) {
    let timeout = Duration::from_secs(5);
    let value = (val >> 16) as u16;
    let index = val as u16;
    let length = buf.len() as u16;

    let _res: Result<usize> = {
        let fut = async {
            let ci = ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request,
                value,
                index,
                length,
            };
            let comp = i.control_in(ci).await;
            comp.status.map_err(std::io::Error::other)?;

            let n = comp.data.len();
            buf[..n].copy_from_slice(&comp.data);
            Ok(n)
        };

        block_on(fut.or(async {
            Timer::after(timeout).await;
            Err(TimedOut.into())
        }))
    };
}

fn dev_info(i: &Interface) {
    let mut buf = [0; 0x20];
    cmd_in(i, &mut buf, EP0_GET_CPU_INFO, 0);
    let reply = from_utf8(&buf).unwrap();
    println!("Device says: {reply}");
}

fn cmd_out(i: &Interface, request: u8, val: u32) {
    let timeout = Duration::from_secs(5);
    let value = (val >> 16) as u16;
    let index = val as u16;

    let _res: Result<()> = {
        let fut = async {
            let co = ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request,
                value,
                index,
                data: &[],
            };
            let comp = i.control_out(co).await;
            comp.status.map_err(std::io::Error::other)?;
            Ok(())
        };

        block_on(fut.or(async {
            Timer::after(timeout).await;
            Err(TimedOut.into())
        }))
    };
}

fn set_code_addr(i: &Interface, addr: u32) {
    cmd_out(i, EP0_SET_DATA_ADDRESS, addr);
}

fn run_code(i: &Interface, addr: u32) {
    cmd_out(i, EP0_PROG_START, addr);
}

fn load(i: &Interface, usb_out_addr: u8, addr: u32, file: &File) {
    set_code_addr(&i, addr);
    let mut reader = BufReader::new(file);
    let mut buf = [0_u8; CHUNK_SIZE];
    loop {
        let len = reader.read(&mut buf[..]).unwrap();
        if len == 0 {
            break;
        }
        let _: Result<()> = {
            let timeout = Duration::from_secs(5);
            let fut = async {
                let comp = i.bulk_out(usb_out_addr, buf[..len].to_vec()).await;
                comp.status.map_err(io::Error::other)?;
                Ok(())
            };

            block_on(fut.or(async {
                Timer::after(timeout).await;
                Err(TimedOut.into())
            }))
        };
    }
}

fn main() {
    let cmd = Cli::parse().cmd;

    let di = nusb::list_devices()
        .unwrap()
        .find(|d| d.vendor_id() == KENDRYTE_VID && d.product_id() == K230D_PID)
        .expect("Device not found, is it connected and in the right mode?");
    let ms = di.manufacturer_string().unwrap();
    let ps = di.product_string().unwrap();
    println!("Found {ms} {ps}");

    // Just use the first interface
    let ii = di.interfaces().next().unwrap().interface_number();
    let d = di.open().unwrap();
    let i = claim_interface(&d, ii).unwrap();

    let speed = di.speed().unwrap();
    let packet_size = match speed {
        Speed::Full | Speed::Low => 64,
        Speed::High => 512,
        Speed::Super | Speed::SuperPlus => 1024,
        _ => panic!("Unknown USB device speed {speed:?}"),
    };
    println!("speed {speed:?} - max packet size: {packet_size}");

    // TODO: Nice error messages when either is not found
    // We may also hardcode the endpoint to 0x01.
    let c = d.configurations().next().unwrap();
    let s = c.interface_alt_settings().next().unwrap();

    let mut es = s.endpoints();
    let e_out = es.find(|e| e.direction() == Direction::Out).unwrap();
    let e_out_addr = e_out.address();

    let mut es = s.endpoints();
    let e_in = es.find(|e| e.direction() == Direction::In).unwrap();
    let e_in_addr = e_in.address();

    dev_info(&i);

    match cmd {
        Command::CpuInfo => {}
        Command::Rom => run_code(&i, MASK_ROM_BASE as u32),
        Command::Load { file_name, address } => {
            let data = File::open(file_name).unwrap();
            load(&i, e_out_addr, address, &data);
        }
        Command::Run { file_name, address } => {
            let data = File::open(file_name).unwrap();
            load(&i, e_out_addr, address, &data);
            run_code(&i, address);
        }
    }
}
