use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::HostId;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    println!("=== ScreenCaptureKit Audio Capture with App Filtering ===\n");

    // 1. Get ScreenCaptureKit Host
    let t0 = Instant::now();
    let host = cpal::host_from_id(HostId::ScreenCaptureKit)
        .expect("ScreenCaptureKit host not found. Are you on macOS 12.3+?");
    println!("[TIMING] Host creation: {:?}", t0.elapsed());

    println!("Host: {:?}", host.id());

    // 2. Get default input device
    let t1 = Instant::now();
    let device = host
        .default_input_device()
        .expect("No input device available");
    println!("[TIMING] default_input_device(): {:?}", t1.elapsed());

    println!("Selected device: {}", device.description()?.name());

    // 3. Configure stream with excluded apps
    let t2 = Instant::now();
    let supported_config = device.default_input_config().unwrap();
    println!("[TIMING] default_input_config(): {:?}", t2.elapsed());

    println!("Default config: {:?}", supported_config);
    let sample_format = supported_config.sample_format();

    // Get StreamConfig and set excluded apps on it
    let mut config: cpal::StreamConfig = supported_config.into();

    // Set excluded apps by name in StreamConfig (the new API!)
    println!("\n=== Setting up app exclusion ===");
    let excluded_apps = vec!["QQ音乐".to_string()]; // 只排除 QQ音乐
    println!("🔇 Will exclude apps matching: {:?}", excluded_apps);
    config.excluded_app_names = Some(excluded_apps);

    // Prepare buffer to store captured audio so we can write it to a WAV file later.
    // We assume f32 samples, matching the `as_slice::<f32>()` below.
    let recorded_samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let recorded_for_cb = recorded_samples.clone();

    // Cache WAV metadata derived from the stream config.
    let wav_channels = config.channels;
    let wav_sample_rate = config.sample_rate;

    // 4. Build input stream (app names will be matched against cached apps here)
    let t3 = Instant::now();
    println!("\nBuilding input stream (will fetch app list if not cached)...");
    let stream = device.build_input_stream_raw(
        &config,
        sample_format,
        move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
            // Process data
            if let Some(samples) = data.as_slice::<f32>() {
                // Append samples to our recording buffer.
                if let Ok(mut buf) = recorded_for_cb.lock() {
                    buf.extend_from_slice(samples);
                }

                let len = samples.len();
                let mut rms = 0.0;
                for &sample in samples {
                    rms += sample * sample;
                }
                rms = (rms / len as f32).sqrt();
                if rms > 0.001 {
                    println!("Received {} samples. RMS: {:.4}", len, rms);
                }
            }
        },
        move |err| {
            eprintln!("Stream error: {:?}", err);
        },
        None, // Timeout
    )?;
    println!("[TIMING] build_input_stream_raw(): {:?}", t3.elapsed());

    println!("Stream built. Playing...");

    let t4 = Instant::now();
    stream.play()?;
    println!("[TIMING] stream.play(): {:?}", t4.elapsed());

    println!("\n=== Total initialization time: {:?} ===\n", t0.elapsed());

    println!("Recording for 10 seconds...");
    println!("🔊 If QQ音乐 is playing, you should see NO audio output");
    println!("🔊 If other apps are playing audio, you WILL see their audio\n");
    std::thread::sleep(std::time::Duration::from_secs(10));

    println!("Stopping...");
    stream.pause()?; // Optional, drop handles it

    // After the demo is complete, dump all captured audio to a WAV file so it can be inspected.
    // The file will be written next to the crate's Cargo.toml.
    {
        let samples = recorded_samples.lock().unwrap();
        if !samples.is_empty() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/macos_screencapture.wav");
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
