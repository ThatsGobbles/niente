#![cfg(test)]

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use byteorder::{ByteOrder, LittleEndian};
use claxon::{Error as ClaxonError, FlacReader, FlacIntoSamples, Result as ClaxonResult};
use claxon::input::BufferedReader;
use sampara::Frame;
use sampara::signal::FromFrames as SignalFromFrames;
use hound::{Error as HoundError, WavReader, WavIntoSamples, SampleFormat, Result as HoundResult};

use crate::MAX_CHANNELS;

pub(crate) enum WaveKind {
    Sine,
    Square,
    Triangle,
    Sawtooth,
}

impl WaveKind {
    const fn name(&self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Square => "square",
            Self::Triangle => "triangle",
            Self::Sawtooth => "sawtooth",
        }
    }
}

fn amplitude(bits_per_sample: u32) -> f64 {
    // Since the samples are signed integers (one of 16/24/32-bit), need to
    // normalize them to the range [-1.0, 1.0).
    let a = match bits_per_sample {
        0 => panic!("bits per sample is 0"),
        b => {
            let shift = b - 1;
            1u32.checked_shl(shift)
                .unwrap_or_else(|| panic!("too many bits per sample (max 32): {}", b))
        },
    };

    a as f64
}

pub(crate) struct FlacFrames<R: Read> {
    samples: FlacIntoSamples<BufferedReader<R>>,
    pub num_channels: u32,
    pub sample_rate: u32,
    amplitude: f64,
}

impl<R: Read> FlacFrames<R> {
    pub fn new(reader: FlacReader<R>) -> Self {
        // Get stream info.
        let info = reader.streaminfo();
        let num_channels = info.channels;
        let bits_per_sample = info.bits_per_sample;
        let sample_rate = info.sample_rate;

        assert!(
            num_channels as usize <= MAX_CHANNELS,
            "too many channels (max {}): {}", MAX_CHANNELS, num_channels,
        );

        let amplitude = amplitude(bits_per_sample);

        let samples = reader.into_samples();

        Self {
            samples,
            num_channels,
            sample_rate,
            amplitude,
        }
    }
}

impl<R: Read> Iterator for FlacFrames<R> {
    type Item = ClaxonResult<[f64; MAX_CHANNELS]>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut frame = [0.0f64; MAX_CHANNELS];

        for (i, f) in frame.channels_mut().enumerate().take(self.num_channels as usize) {
            let raw_sample = match self.samples.next() {
                Some(Ok(x)) => x,
                Some(Err(e)) => return Some(Err(e)),
                None if i == 0 => return None,
                None => return Some(Err(ClaxonError::FormatError("incomplete frame at end of stream"))),
            };

            let normalized_sample = raw_sample as f64 / self.amplitude;

            *f = normalized_sample
        }

        Some(Ok(frame))
    }
}

enum WavNormedSamples<R: Read> {
    // Use i32, which will accommodate i8, i16, and i32.
    // Also include the normalization factor (aka "amplitude").
    Int(WavIntoSamples<R, i32>, f64),

    // Use f32, as it is the only supported float type.
    Float(WavIntoSamples<R, f32>),
}

impl<R: Read> Iterator for WavNormedSamples<R> {
    type Item = HoundResult<f64>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Int(i_samples, amp) => Some(i_samples.next()?.map(|i| i as f64 / *amp)),
            Self::Float(f_samples) => Some(f_samples.next()?.map(|f| f as f64)),
        }
    }
}

pub(crate) struct WavFrames<R: Read> {
    samples: WavNormedSamples<R>,
    pub num_channels: u32,
    pub sample_rate: u32,
}

impl<R: Read> WavFrames<R> {
    pub fn new(reader: WavReader<R>) -> Self {
        // Get stream info.
        let info = reader.spec();
        let num_channels = info.channels as u32;
        let bits_per_sample = info.bits_per_sample as u32;
        let sample_rate = info.sample_rate;

        assert!(
            num_channels as usize <= MAX_CHANNELS,
            "too many channels (max {}): {}", MAX_CHANNELS, num_channels,
        );

        let samples = match info.sample_format {
            SampleFormat::Int => WavNormedSamples::Int(
                reader.into_samples(),
                amplitude(bits_per_sample),
            ),
            SampleFormat::Float => WavNormedSamples::Float(reader.into_samples()),
        };

        Self {
            samples,
            num_channels,
            sample_rate,
        }
    }
}

impl<R: Read> Iterator for WavFrames<R> {
    type Item = HoundResult<[f64; MAX_CHANNELS]>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut frame = [0.0f64; MAX_CHANNELS];

        for (i, f) in frame.channels_mut().enumerate().take(self.num_channels as usize) {
            let normed_sample = match self.samples.next() {
                Some(Ok(x)) => x,
                Some(Err(e)) => return Some(Err(e)),
                None if i == 0 => return None,
                None => return Some(Err(HoundError::FormatError("incomplete frame at end of stream"))),
            };

            *f = normed_sample
        }

        Some(Ok(frame))
    }
}

pub(crate) enum TestReader<R: Read> {
    Flac(FlacFrames<R>),
    Wav(WavFrames<R>),
}

impl TestReader<File> {
    pub fn read_path(path: &Path) -> Self {
        let ext = path.extension()
            .unwrap_or_else(|| panic!("path does not have an extension: {}", path.display()));

        if ext == "flac" {
            Self::Flac(TestUtil::load_flac_data(path))
        } else if ext == "wav" {
            Self::Wav(TestUtil::load_wav_data(path))
        } else {
            panic!("unknown extension: {:?}", ext);
        }
    }

    pub fn into_signal(self) -> SignalFromFrames<Self, MAX_CHANNELS> {
        sampara::signal::from_frames(self)
    }
}

impl<R: Read> Iterator for TestReader<R> {
    type Item = [f64; MAX_CHANNELS];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Flac(fs) => fs.next().map(Result::unwrap),
            Self::Wav(fs) => fs.next().map(Result::unwrap),
        }
    }
}

pub(crate) struct TestUtil;

impl TestUtil {
    pub fn check_sox() -> bool {
        Command::new("sox").arg("--version")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn load_flac_data(path: &Path) -> FlacFrames<File> {
        let file = File::open(path)
            .unwrap_or_else(|e| panic!("could not open file: {}", e));

        let reader = FlacReader::new(file)
            .unwrap_or_else(|e| panic!("could not read FLAC data: {}", e));

        FlacFrames::new(reader)
    }

    pub fn load_wav_data(path: &Path) -> WavFrames<File> {
        let file = File::open(path)
            .unwrap_or_else(|e| panic!("could not open file: {}", e));

        let reader = WavReader::new(file)
            .unwrap_or_else(|e| panic!("could not read WAV data: {}", e));

        WavFrames::new(reader)
    }

    pub fn load_custom_audio_paths(dir_path: &Path) -> Vec<PathBuf> {
        let read_dir = std::fs::read_dir(dir_path)
            .unwrap_or_else(|e| panic!("could not read dir: {}", e));

        let mut entries = read_dir
            .filter_map(|res| {
                let dir_entry = res
                    .unwrap_or_else(|e| panic!("could not read dir entry: {}", e));

                let path = dir_entry.path();

                match path.extension() {
                    Some(ext) => {
                        if ext == "flac" || ext == "wav" {
                            Some(path)
                        } else {
                            None
                        }
                    },
                    None => None,
                }
            })
            .collect::<Vec<_>>()
        ;

        entries.sort_by(|ea, eb| ea.file_name().cmp(&eb.file_name()));

        entries
    }

    pub fn sox_eval(cmd: &mut Command) -> Vec<u8> {
        let output = cmd.output()
            .unwrap_or_else(|e| panic!("failed to execute command: {}", e));

        let Output { status, stdout, stderr } = output;

        assert!(status.success(), "command returned with non-zero code: {}", status);
        assert!(
            stderr.len() == 0,
            "non-empty stderr from running sox as subprocess: {}",
            std::str::from_utf8(&stderr).unwrap(),
        );

        stdout
    }

    pub fn sox_eval_string(cmd: &mut Command) -> String {
        let raw_stdout = Self::sox_eval(cmd);

        String::from_utf8(raw_stdout)
            .unwrap_or_else(|e| panic!("cannot convert stdout bytes into string: {}", e))
    }

    pub fn sox_eval_samples(cmd: &mut Command) -> Vec<f64> {
        let raw_stdout = Self::sox_eval(cmd);

        let mut res = vec![0.0f64; raw_stdout.len() / std::mem::size_of::<f64>()];
        LittleEndian::read_f64_into(&raw_stdout, &mut res);

        res
    }

    pub fn sox_gen_wave_cmd(sample_rate: u32, kind: &WaveKind, frequency: u32) -> Command {
        let mut cmd = Command::new("sox");

        cmd
            // No input file name.
            .arg("--null")

            // Set sample rate.
            .arg("--rate").arg(sample_rate.to_string())

            // Set output data format params.
            .arg("--endian").arg("little")
            .arg("--channels").arg("1")
            .arg("--type").arg("f64")

            // Output to stdout.
            .arg("-")

            // Wave to generate/synthesize.
            .arg("synth").arg("3").arg(kind.name()).arg(frequency.to_string())

            // Insert some headroom to prevent clipping.
            .arg("gain").arg("-2")
        ;

        cmd
    }

    fn sox_gen_wave(sample_rate: u32, kind: &WaveKind, frequency: u32) -> Vec<f64> {
        Self::sox_eval_samples(&mut Self::sox_gen_wave_cmd(sample_rate, kind, frequency))
    }

    pub fn load_audio_data(path: &Path) -> (Vec<f64>, u32, u8) {
        // Get sample rate.
        let stdout_str = Self::sox_eval_string(
            Command::new("soxi").arg("-r").arg(path)
        );
        let sample_rate = str::parse::<u32>(&stdout_str).unwrap();

        // Get num channels.
        let stdout_str = Self::sox_eval_string(
            Command::new("soxi").arg("-c").arg(path)
        );
        let num_channels = str::parse::<u8>(&stdout_str).unwrap();

        // Read the audio data.
        let flat_samples = Self::sox_eval_samples(
            Command::new("sox")
                .arg(path)

                // Set output data format params.
                .arg("--endian").arg("little")
                .arg("--type").arg("f64")

                // Output to stdout.
                .arg("-")
        );

        (flat_samples, sample_rate, num_channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::File;

    use tempfile::{TempDir, Builder};

    fn create_dir_with_files<I, S>(files: I) -> TempDir
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let temp_dir = Builder::new().tempdir().unwrap();

        let path = temp_dir.path();

        for file_name in files {
            File::create(path.join(file_name.as_ref())).unwrap();
        }

        temp_dir
    }

    #[test]
    fn load_custom_audio_paths() {
        let temp_dir = create_dir_with_files(&[
            "03.flac", "02.txt", "01.wav", "04"
        ]);
        let temp_dir_path = temp_dir.path();

        assert_eq!(
            TestUtil::load_custom_audio_paths(&temp_dir_path),
            vec![
                temp_dir_path.join("01.wav"),
                temp_dir_path.join("03.flac"),
            ]
        );
    }
}
