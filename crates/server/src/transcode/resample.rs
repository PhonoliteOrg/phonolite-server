//! Bounded streaming, band-limited conversion on an absolute rational frame grid.
//!
//! The grid is shared by independently requested ranges. Integer phase accounting
//! prevents cumulative drift; lookahead is retained until the next input chunk,
//! and `finish` emits the exact rounded duration without adding a filter tail.

const PHASES: usize = 1024;

pub(super) struct SincResampler {
    input_rate: u32,
    output_rate: u32,
    channels: usize,
    radius: i64,
    origin: u64,
    buffered_from: u64,
    input_end: u64,
    next_output: u64,
    buffer: Vec<i16>,
    kernels: Vec<Vec<f64>>,
}

impl SincResampler {
    pub(super) fn new(input_rate: u32, output_rate: u32, channels: u8, origin: u64) -> Self {
        assert!(input_rate > 0 && output_rate > 0 && channels > 0);
        let ratio = (f64::from(output_rate) / f64::from(input_rate)).min(1.0);
        let radius = (32.0 / ratio).ceil().clamp(32.0, 256.0) as i64;
        let cutoff = ratio * 0.94;
        let kernels = (0..=PHASES)
            .map(|phase| {
                let fraction = phase as f64 / PHASES as f64;
                let mut kernel: Vec<f64> = (-radius + 1..=radius)
                    .map(|offset| {
                        let distance = offset as f64 - fraction;
                        let argument = std::f64::consts::PI * cutoff * distance;
                        let sinc = if argument.abs() < 1e-12 {
                            1.0
                        } else {
                            argument.sin() / argument
                        };
                        let window = 0.42
                            + 0.5 * (std::f64::consts::PI * distance / radius as f64).cos()
                            + 0.08 * (2.0 * std::f64::consts::PI * distance / radius as f64).cos();
                        cutoff * sinc * window
                    })
                    .collect();
                let sum: f64 = kernel.iter().sum();
                for weight in &mut kernel {
                    *weight /= sum;
                }
                kernel
            })
            .collect();
        Self {
            input_rate,
            output_rate,
            channels: usize::from(channels),
            radius,
            origin,
            buffered_from: origin,
            input_end: origin,
            next_output: (u128::from(origin) * u128::from(output_rate))
                .div_ceil(u128::from(input_rate)) as u64,
            buffer: Vec::new(),
            kernels,
        }
    }

    pub(super) fn first_output_frame(&self) -> u64 {
        self.next_output
    }

    pub(super) fn process(&mut self, input: &[i16]) -> Vec<i16> {
        assert_eq!(input.len() % self.channels, 0);
        self.input_end += (input.len() / self.channels) as u64;
        self.buffer.extend_from_slice(input);
        self.produce(false)
    }

    pub(super) fn finish(&mut self) -> Vec<i16> {
        self.produce(true)
    }

    fn produce(&mut self, finished: bool) -> Vec<i16> {
        let mut output = Vec::new();
        if self.buffer.is_empty() {
            return output;
        }
        let final_output = ((u128::from(self.input_end) * u128::from(self.output_rate)
            + u128::from(self.input_rate / 2))
            / u128::from(self.input_rate)) as u64;
        while self.next_output < final_output {
            let numerator = u128::from(self.next_output) * u128::from(self.input_rate);
            let center = (numerator / u128::from(self.output_rate)) as i64;
            if !finished && center + self.radius >= self.input_end as i64 {
                break;
            }
            let phase_scaled = (numerator % u128::from(self.output_rate)) * PHASES as u128;
            let phase = (phase_scaled / u128::from(self.output_rate)) as usize;
            let blend =
                (phase_scaled % u128::from(self.output_rate)) as f64 / f64::from(self.output_rate);
            for channel in 0..self.channels {
                let mut sample = 0.0;
                for (tap, offset) in (-self.radius + 1..=self.radius).enumerate() {
                    let weight = self.kernels[phase][tap]
                        + blend * (self.kernels[phase + 1][tap] - self.kernels[phase][tap]);
                    let frame = (center + offset)
                        .clamp(self.origin as i64, self.input_end as i64 - 1)
                        as u64;
                    let index = (frame - self.buffered_from) as usize * self.channels + channel;
                    sample += f64::from(self.buffer[index]) * weight;
                }
                output.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            }
            self.next_output += 1;
        }
        let next_center = (u128::from(self.next_output) * u128::from(self.input_rate)
            / u128::from(self.output_rate)) as u64;
        let discard_until = next_center
            .saturating_sub(self.radius as u64)
            .max(self.buffered_from)
            .min(self.input_end);
        let discard = (discard_until - self.buffered_from) as usize * self.channels;
        self.buffer.drain(..discard);
        self.buffered_from = discard_until;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_chunk_boundaries_preserve_every_output_frame_and_phase() {
        let input: Vec<i16> = (0..4417)
            .map(|n| ((n as f64 * 0.071).sin() * 20000.0) as i16)
            .collect();
        let mut whole = SincResampler::new(44100, 48000, 1, 0);
        let mut expected = whole.process(&input);
        expected.extend(whole.finish());
        let mut chunks = SincResampler::new(44100, 48000, 1, 0);
        let mut actual = Vec::new();
        for chunk in input.chunks(17) {
            actual.extend(chunks.process(chunk));
        }
        actual.extend(chunks.finish());
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), (input.len() * 48000 + 22050) / 44100);
    }

    #[test]
    fn downsampling_rejects_aliases_and_retains_dc_and_short_tails() {
        let tone: Vec<i16> = (0..9600)
            .map(|n| {
                ((n as f64 * 2.0 * std::f64::consts::PI * 36000.0 / 96000.0).sin() * 20000.0) as i16
            })
            .collect();
        let mut sampler = SincResampler::new(96000, 48000, 1, 0);
        let mut output = sampler.process(&tone);
        output.extend(sampler.finish());
        let rms = (output[100..output.len() - 100]
            .iter()
            .map(|n| f64::from(*n).powi(2))
            .sum::<f64>()
            / (output.len() - 200) as f64)
            .sqrt();
        assert!(rms < 30.0, "aliased RMS: {rms}");
        let mut short = SincResampler::new(44100, 48000, 1, 0);
        assert!(short.process(&[1234; 7]).is_empty());
        let tail = short.finish();
        assert_eq!(tail, vec![1234; 8]);
    }

    #[test]
    fn independently_started_ranges_share_the_absolute_grid() {
        let input: Vec<i16> = (0..44100)
            .map(|n| ((n as f64 * 0.04).sin() * 15000.0) as i16)
            .collect();
        let mut full = SincResampler::new(44100, 48000, 1, 0);
        let mut expected = full.process(&input);
        expected.extend(full.finish());
        let origin = 17003;
        let mut range = SincResampler::new(44100, 48000, 1, origin);
        let first = range.first_output_frame() as usize;
        let mut output = range.process(&input[origin as usize..]);
        output.extend(range.finish());
        assert_eq!(
            output[100..output.len() - 100],
            expected[first + 100..expected.len() - 100]
        );
    }
}
