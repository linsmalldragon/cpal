//! Example demonstrating dynamic excluded_apps update during audio capture
//!
//! This example shows how to:
//! 1. Start audio capture with no exclusions
//! 2. Dynamically exclude an app (e.g., QQ音乐) during capture
//! 3. Re-include the app by clearing exclusions
//!
//! All without interrupting the audio stream!

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::HostId;
use std::time::{Duration, Instant};

fn main() -> Result<(), anyhow::Error> {
    #[cfg(not(target_os = "macos"))]
    {
        println!("This example is only for macOS.");
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        run_macos_example()
    }
}

#[cfg(target_os = "macos")]
fn run_macos_example() -> Result<(), anyhow::Error> {
    use cpal::platform::{ScreenCaptureKitDevice, ScreenCaptureKitStream};

    println!("=== Dynamic Excluded Apps Update Demo ===\n");

    // 1. Get ScreenCaptureKit Host
    let host = cpal::host_from_id(HostId::ScreenCaptureKit)
        .expect("ScreenCaptureKit host not found. Are you on macOS 12.3+?");

    // 2. Get default input device (no exclusions initially)
    let device = host
        .default_input_device()
        .expect("No input device available");

    let sck_device: ScreenCaptureKitDevice = match device.into_inner() {
        cpal::platform::DeviceInner::ScreenCaptureKit(d) => d,
        _ => panic!("Expected ScreenCaptureKit device"),
    };

    println!("Device: {}", sck_device.description()?.name());

    // 3. Build stream with NO exclusions initially
    // Note: build_input_stream_raw on ScreenCaptureKitDevice returns ScreenCaptureKitStream directly
    let supported_config = sck_device.default_input_config()?;
    let sample_format = supported_config.sample_format();
    let config: cpal::StreamConfig = supported_config.into();

    let stream: ScreenCaptureKitStream = sck_device.build_input_stream_raw(
        &config,
        sample_format,
        move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
            if let Some(samples) = data.as_slice::<f32>() {
                let len = samples.len();
                let rms: f32 = (samples.iter().map(|s| s * s).sum::<f32>() / len as f32).sqrt();
                if rms > 0.001 {
                    println!("  📊 RMS: {:.4} ({} samples)", rms, len);
                }
            }
        },
        move |err| {
            eprintln!("Stream error: {:?}", err);
        },
        None,
    )?;

    stream.play()?;
    println!("Stream started!\n");

    // Phase 1: Capture all audio (5 seconds)
    println!("=== Phase 1: Capturing ALL audio (5 seconds) ===");
    println!("🔊 You should see audio from ALL applications\n");
    std::thread::sleep(Duration::from_secs(5));

    // Phase 2: Exclude QQ音乐 dynamically
    println!("\n=== Phase 2: Excluding QQ音乐 (5 seconds) ===");
    let t0 = Instant::now();
    match stream.update_excluded_apps_by_names(&["QQ音乐"]) {
        Ok(()) => {
            println!(
                "✅ Filter updated in {:?} - QQ音乐 is now EXCLUDED",
                t0.elapsed()
            );
            println!("🔇 You should NOT see audio from QQ音乐\n");
        }
        Err(e) => {
            println!("❌ Failed to update filter: {:?}", e);
        }
    }
    std::thread::sleep(Duration::from_secs(5));

    // Phase 3: Clear exclusions (re-include all apps)
    println!("\n=== Phase 3: Clearing exclusions (5 seconds) ===");
    let t1 = Instant::now();
    match stream.update_excluded_apps_by_names(&[]) {
        Ok(()) => {
            println!(
                "✅ Filter updated in {:?} - All apps are now INCLUDED",
                t1.elapsed()
            );
            println!("🔊 You should see audio from ALL applications again\n");
        }
        Err(e) => {
            println!("❌ Failed to update filter: {:?}", e);
        }
    }
    std::thread::sleep(Duration::from_secs(5));

    // Phase 4: Exclude by PID (demonstrating refresh_applications_cache)
    println!("\n=== Phase 4: Demonstrate PID-based exclusion ===");

    // Refresh the app cache to get latest running apps
    match ScreenCaptureKitStream::refresh_applications_cache() {
        Ok(apps) => {
            println!("Found {} running applications:", apps.len());

            // Find QQ音乐 and get its PID
            for app in &apps {
                let name = unsafe { app.applicationName().to_string() };
                let pid = unsafe { app.processID() };
                if name.contains("QQ") || name.contains("音乐") {
                    println!("  🎵 {} (PID: {})", name, pid);

                    // Exclude by PID
                    let t2 = Instant::now();
                    match stream.update_excluded_apps_by_pids(&[pid]) {
                        Ok(()) => {
                            println!("  ✅ Excluded by PID in {:?}", t2.elapsed());
                        }
                        Err(e) => {
                            println!("  ❌ Failed: {:?}", e);
                        }
                    }
                    break;
                }
            }
        }
        Err(e) => {
            println!("❌ Failed to refresh apps cache: {:?}", e);
        }
    }

    println!("\n🔇 If QQ音乐 was found, it's now excluded by PID");
    std::thread::sleep(Duration::from_secs(3));

    println!("\n=== Demo Complete! ===");
    stream.pause()?;

    Ok(())
}
