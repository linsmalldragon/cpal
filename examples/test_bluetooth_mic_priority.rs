//! Test for default_input_device_bluetooth_mic_speaker_last method
//!
//! This example lists all input devices with their interface types and directions,
//! then tests the new method to verify that Bluetooth duplex devices (like AirPods Pro)
//! are deprioritized in favor of built-in microphones.
//!
//! Run with: cargo run --example test_bluetooth_mic_priority

use cpal::traits::{DeviceTrait, HostTrait};

fn print_device_info(device: &cpal::Device, idx: usize) {
    match device.description() {
        Ok(desc) => {
            println!("Device #{}: {}", idx, desc.name());
            println!("  Interface Type: {:?}", desc.interface_type());
            println!("  Device Type: {:?}", desc.device_type());
            println!("  Direction: {:?}", desc.direction());
            println!("  Supports Input: {}", desc.supports_input());
            println!("  Supports Output: {}", desc.supports_output());
            if let Some(manufacturer) = desc.manufacturer() {
                println!("  Manufacturer: {}", manufacturer);
            }
            if let Some(address) = desc.address() {
                println!("  Address: {}", address);
            }
            // Print extended info if available
            let extended = desc.extended();
            if !extended.is_empty() {
                println!("  Extended info:");
                for line in extended {
                    println!("    {}", line);
                }
            }
        }
        Err(e) => {
            println!("Device #{}: Error getting description: {:?}", idx, e);
        }
    }
    println!();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();

    println!("=== All Devices (devices()) ===\n");

    // List ALL devices to see the full picture
    let all_devices: Vec<_> = host.devices()?.collect();
    for (idx, device) in all_devices.iter().enumerate() {
        print_device_info(device, idx + 1);
    }

    println!("=== All Input Devices (input_devices()) ===\n");

    // List all input devices with their properties
    let input_devices: Vec<_> = host.input_devices()?.collect();

    for (idx, device) in input_devices.iter().enumerate() {
        print_device_info(device, idx + 1);
    }

    println!("=== Testing default_input_device_bluetooth_mic_speaker_last ===\n");

    // Test the new method
    match host.default_input_device_bluetooth_mic_speaker_last() {
        Some(device) => {
            match device.description() {
                Ok(desc) => {
                    println!("Selected device: {}", desc.name());
                    println!("  Interface Type: {:?}", desc.interface_type());
                    println!("  Device Type: {:?}", desc.device_type());
                    println!("  Direction: {:?}", desc.direction());

                    // Check if this is what we expect
                    let is_bluetooth = desc.interface_type() == cpal::InterfaceType::Bluetooth;
                    let is_duplex = desc.direction() == cpal::DeviceDirection::Duplex;

                    if is_bluetooth && is_duplex {
                        println!("\n⚠️  WARNING: Selected device is a Bluetooth duplex device!");
                        println!("    This means there are no other input devices available.");
                    } else {
                        println!("\n✅ SUCCESS: Selected device is NOT a Bluetooth duplex device.");
                        println!("    The method correctly prioritized non-Bluetooth/non-duplex devices.");
                    }
                }
                Err(e) => {
                    println!("Selected device: Error getting description: {:?}", e);
                }
            }
        }
        None => {
            println!("No input device available!");
        }
    }

    println!("\n=== Comparison with standard default_input_device ===\n");

    // Compare with standard method
    match host.default_input_device() {
        Some(device) => {
            match device.description() {
                Ok(desc) => {
                    println!("Standard default device: {}", desc.name());
                    println!("  Interface Type: {:?}", desc.interface_type());
                    println!("  Device Type: {:?}", desc.device_type());
                    println!("  Direction: {:?}", desc.direction());
                }
                Err(e) => {
                    println!("Standard default device: Error getting description: {:?}", e);
                }
            }
        }
        None => {
            println!("No default input device available!");
        }
    }

    Ok(())
}

