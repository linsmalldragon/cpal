//! Example demonstrating dynamic excluded_apps update during audio capture
//!
//! This example shows how to:
//! 1. Start audio capture with no exclusions
//! 2. Dynamically exclude an app (e.g., QQ音乐) during capture
//! 3. Re-include the app by clearing exclusions
//!
//! All without interrupting the audio stream!
//!
//! **Note**: This example uses the generic `cpal::Stream` type, which works with
//! any backend. The dynamic update methods are only available for ScreenCaptureKit
//! streams on macOS, but the API is the same regardless of the backend.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::HostId;
use std::sync::{Arc, Mutex};
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
    println!("=== Dynamic Excluded Apps Update Demo ===\n");

    // 1. Get ScreenCaptureKit Host
    let host = cpal::host_from_id(HostId::ScreenCaptureKit)
        .expect("ScreenCaptureKit host not found. Are you on macOS 12.3+?");

    // 2. Get default input device (no exclusions initially)
    let device = host
        .default_input_device()
        .expect("No input device available");

    println!("Device: {}", device.description()?.name());

    // 3. Build stream with NO exclusions initially
    // Using generic cpal::Stream (works with any backend)
    let supported_config = device.default_input_config()?;
    let sample_format = supported_config.sample_format();
    let config: cpal::StreamConfig = supported_config.into();

    // Prepare buffer to store captured audio so we can write it to a WAV file later.
    // We assume f32 samples, matching the `as_slice::<f32>()` below.
    let recorded_samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let recorded_for_cb = recorded_samples.clone();

    // Cache WAV metadata derived from the stream config.
    let wav_channels = config.channels;
    let wav_sample_rate = config.sample_rate;

    let stream = device.build_input_stream_raw(
        &config,
        sample_format,
        move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
            if let Some(samples) = data.as_slice::<f32>() {
                // Append samples to our recording buffer.
                if let Ok(mut buf) = recorded_for_cb.lock() {
                    buf.extend_from_slice(samples);
                }

                // Simple RMS meter for visual feedback.
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

    // Optional: Check if this is a ScreenCaptureKit stream
    // (The update methods will return an error for other stream types)
    if stream.is_screencapturekit() {
        println!("✅ This is a ScreenCaptureKit stream - dynamic updates are supported\n");
    } else {
        println!("⚠️  This is not a ScreenCaptureKit stream - dynamic updates may not work\n");
    }

    // Phase 1: Capture all audio (5 seconds)
    println!("=== Phase 1: Capturing ALL audio (5 seconds) ===");
    println!("🔊 You should see audio from ALL applications\n");
    std::thread::sleep(Duration::from_secs(5));

    // Phase 2: Exclude QQ音乐 dynamically using bundle ID (recommended)
    // Bundle ID is more reliable than app name as it doesn't change with locale
    // Find bundle ID: mdls -name kMDItemCFBundleIdentifier /Applications/QQMusic.app
    println!("\n=== Phase 2: Excluding QQ音乐 by Bundle ID (5 seconds) ===");
    let t0 = Instant::now();
    match stream.update_excluded_apps_by_bundle_ids(&["com.tencent.QQMusicMac"]) {
        Ok(()) => {
            println!(
                "✅ Filter updated in {:?} - QQ音乐 is now EXCLUDED (via bundle ID)",
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
    match stream.update_excluded_apps_by_bundle_ids(&[]) {
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

    // Phase 4: Exclude by bundle ID (demonstrating refresh_applications_cache)
    println!("\n=== Phase 4: Demonstrate bundle ID-based exclusion ===");

    // To refresh the app cache, we need to access the underlying ScreenCaptureKitStream
    // This is useful if you want to detect newly launched applications
    use cpal::platform::ScreenCaptureKitStream;
    match ScreenCaptureKitStream::refresh_applications_cache() {
        Ok(apps) => {
            println!("Found {} running applications:", apps.len());

            // Find QQ音乐 and get its bundle ID
            for app in &apps {
                let name = unsafe { app.applicationName().to_string() };
                let bundle_id = unsafe { app.bundleIdentifier().to_string() };
                if name.contains("QQ") || name.contains("音乐") || bundle_id.contains("QQMusic") {
                    println!("  🎵 {} (Bundle ID: {})", name, bundle_id);

                    // Exclude by bundle ID using the generic Stream API
                    let t2 = Instant::now();
                    match stream.update_excluded_apps_by_bundle_ids(&[&bundle_id]) {
                        Ok(()) => {
                            println!("  ✅ Excluded by bundle ID in {:?}", t2.elapsed());
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

    println!("\n🔇 If QQ音乐 was found, it's now excluded by bundle ID");
    std::thread::sleep(Duration::from_secs(3));

    println!("\n=== Demo Complete! ===");
    if let Err(e) = stream.pause() {
        eprintln!("Warning: failed to pause stream: {e:?}");
    }

    // After the demo is complete, dump all captured audio to a WAV file so it can be inspected.
    // The file will be written next to the crate's Cargo.toml.
    {
        let samples = recorded_samples.lock().unwrap();
        if !samples.is_empty() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/dynamic_filter_update.wav");
            println!("Writing captured audio to WAV file: {path}");

            let spec = hound::WavSpec {
                channels: wav_channels,
                sample_rate: wav_sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };

            let mut writer = hound::WavWriter::create(path, spec)?;
            for &s in samples.iter() {
                writer.write_sample(s)?;
            }
            writer.finalize()?;

            println!("✅ Saved captured audio to {path}");
        } else {
            println!("⚠️ No audio samples were captured, skipping WAV export");
        }
    }

    Ok(())
}
