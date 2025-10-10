use std::f32::consts::PI;

const TONE_TABLE: [[Tone; 4]; 4] = [
    [Tone::One, Tone::Two, Tone::Three, Tone::A],
    [Tone::Four, Tone::Five, Tone::Six, Tone::B],
    [Tone::Seven, Tone::Eight, Tone::Nine, Tone::C],
    [Tone::Asterisk, Tone::Zero, Tone::Pound, Tone::D],
];

const SAMPLE_RATE: u32 = 8000;
const NUM_SAMPLES: usize = 200;
// how long we need to remain in a state for it to count
const STATE_DURATION: u32 = 2;
const LOW_FREQS: [f32; 4] = [697.0, 770.0, 852.0, 941.0];
const HIGH_FREQS: [f32; 4] = [1209.0, 1336.0, 1477.0, 1633.0];

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum State {
    On,
    Off,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum Tone {
    Zero,
    One,
    Two,
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    A,
    B,
    C,
    D,
    Asterisk,
    Pound,
}

impl Tone {
    pub fn as_char(&self) -> char {
        use Tone::*;

        match self {
            Zero => '0',
            One => '1',
            Two => '2',
            Three => '3',
            Four => '4',
            Five => '5',
            Six => '6',
            Seven => '7',
            Eight => '8',
            Nine => '9',
            A => 'A',
            B => 'B',
            C => 'C',
            D => 'D',
            Asterisk => '*',
            Pound => '#',
        }
    }

    pub fn from_char(c: char) -> Option<Self> {
        let tone = match c {
            '0' => Tone::Zero,
            '1' => Tone::One,
            '2' => Tone::Two,
            '3' => Tone::Three,
            '4' => Tone::Four,
            '5' => Tone::Five,
            '6' => Tone::Six,
            '7' => Tone::Seven,
            '8' => Tone::Eight,
            '9' => Tone::Nine,
            'A' | 'a' => Tone::A,
            'B' | 'b' => Tone::B,
            'C' | 'c' => Tone::C,
            'D' | 'd' => Tone::D,
            '*' => Tone::Asterisk,
            '#' => Tone::Pound,
            _ => return None,
        };

        Some(tone)
    }

    pub fn frequency(&self) -> (usize, usize) {
        match self {
            Tone::One => (LOW_FREQS[0] as usize, HIGH_FREQS[0] as usize),
            Tone::Two => (LOW_FREQS[0] as usize, HIGH_FREQS[1] as usize),
            Tone::Three => (LOW_FREQS[0] as usize, HIGH_FREQS[2] as usize),
            Tone::A => (LOW_FREQS[0] as usize, HIGH_FREQS[3] as usize),
            Tone::Four => (LOW_FREQS[1] as usize, HIGH_FREQS[0] as usize),
            Tone::Five => (LOW_FREQS[1] as usize, HIGH_FREQS[1] as usize),
            Tone::Six => (LOW_FREQS[1] as usize, HIGH_FREQS[2] as usize),
            Tone::B => (LOW_FREQS[1] as usize, HIGH_FREQS[3] as usize),
            Tone::Seven => (LOW_FREQS[2] as usize, HIGH_FREQS[0] as usize),
            Tone::Eight => (LOW_FREQS[2] as usize, HIGH_FREQS[1] as usize),
            Tone::Nine => (LOW_FREQS[2] as usize, HIGH_FREQS[2] as usize),
            Tone::C => (LOW_FREQS[2] as usize, HIGH_FREQS[3] as usize),
            Tone::Asterisk => (LOW_FREQS[3] as usize, HIGH_FREQS[0] as usize),
            Tone::Zero => (LOW_FREQS[3] as usize, HIGH_FREQS[1] as usize),
            Tone::Pound => (LOW_FREQS[3] as usize, HIGH_FREQS[2] as usize),
            Tone::D => (LOW_FREQS[3] as usize, HIGH_FREQS[3] as usize),
        }
    }
}

/// Set up parameters (and some precomputed values for those).
#[derive(Clone, Copy)]
pub struct Parameters {
    // Parameters we're working with
    window_size: usize,
    // Precomputed value:
    sine: f32,
    cosine: f32,
    term_coefficient: f32,
}

pub struct Partial {
    params: Parameters,
    count: usize,
    prev: f32,
    prevprev: f32,
}

impl Parameters {
    pub fn new(target_freq: f32, sample_rate: u32, window_size: usize) -> Self {
        let k = target_freq * (window_size as f32) / (sample_rate as f32);
        let omega = (PI * 2. * k) / (window_size as f32);
        let cosine = libm::cosf(omega);
        Parameters {
            window_size,
            sine: libm::sinf(omega),
            cosine,
            term_coefficient: 2. * cosine,
        }
    }

    pub fn start(self) -> Partial {
        Partial {
            params: self,
            count: 0,
            prev: 0.,
            prevprev: 0.,
        }
    }

    pub fn mag(self, samples: &[f32]) -> f32 {
        self.start().add_samples(samples).finish_mag()
    }

    pub fn mag_squared(self, samples: &[f32]) -> f32 {
        self.start().add_samples(samples).finish_mag_squared()
    }
}

impl Partial {
    pub fn add_samples(mut self, samples: &[f32]) -> Self {
        for &sample in samples {
            let this =
                self.params.term_coefficient * self.prev - self.prevprev + sample;
            self.prevprev = self.prev;
            self.prev = this;
        }
        self.count += samples.len();
        self
    }
    pub fn finish(self) -> (f32, f32) {
        assert_eq!(self.count, self.params.window_size);
        let real = self.prev - self.prevprev * self.params.cosine;
        let imag = self.prevprev * self.params.sine;
        (real, imag)
    }

    pub fn finish_mag(self) -> f32 {
        libm::sqrtf(self.finish_mag_squared())
    }

    pub fn finish_mag_squared(self) -> f32 {
        let (real, imag) = self.finish();
        real * real + imag * imag
    }
}

pub struct DtmfDecoder {
    lows: [Parameters; 4],
    highs: [Parameters; 4],
    low_harms: [Parameters; 4],
    high_harms: [Parameters; 4],

    tone_changes: Vec<(Tone, State)>,

    cur_tone: Option<Tone>,
    old_tone: Option<Tone>,
    // last tone that we sent through the closure
    old_old_tone: Option<Tone>,

    // how long we've been in the current state
    state_duration: u32,

    // holds samples that haven't been processed yet
    samples: [f32; NUM_SAMPLES],
    sample_len: usize,
}

impl Default for DtmfDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DtmfDecoder {
    pub fn new() -> Self {
        let lows = [
            Parameters::new(LOW_FREQS[0], SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(LOW_FREQS[1], SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(LOW_FREQS[2], SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(LOW_FREQS[3], SAMPLE_RATE, NUM_SAMPLES),
        ];

        let highs = [
            Parameters::new(HIGH_FREQS[0], SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(HIGH_FREQS[1], SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(HIGH_FREQS[2], SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(HIGH_FREQS[3], SAMPLE_RATE, NUM_SAMPLES),
        ];

        let low_harms = [
            Parameters::new(LOW_FREQS[0] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(LOW_FREQS[1] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(LOW_FREQS[2] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(LOW_FREQS[3] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
        ];

        let high_harms = [
            Parameters::new(HIGH_FREQS[0] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(HIGH_FREQS[1] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(HIGH_FREQS[2] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
            Parameters::new(HIGH_FREQS[3] * 2.0, SAMPLE_RATE, NUM_SAMPLES),
        ];

        Self {
            lows,
            highs,

            low_harms,
            high_harms,

            tone_changes: Vec::new(),

            cur_tone: None,
            old_tone: None,
            old_old_tone: None,

            state_duration: STATE_DURATION,

            samples: [0.0; NUM_SAMPLES],
            sample_len: 0,
        }
    }

    /// Expects samples to be between -1.0 and 1.0
    /// Batch together as many (or as few) samples when processing
    pub fn process(&mut self, samples: &[f32]) -> Vec<(Tone, State)> {
        for sample in samples {
            self.samples[self.sample_len] = *sample;
            self.sample_len += 1;
            if self.sample_len == NUM_SAMPLES {
                self.process_chunk();
                self.sample_len = 0;
            }
        }
        std::mem::take(&mut self.tone_changes)
    }

    fn process_chunk(&mut self) {
        let mut low_powers = [0.0; 4];
        for (i, lp) in low_powers.iter_mut().enumerate() {
            *lp = self.lows[i].mag_squared(&self.samples);
        }

        let mut high_powers = [0.0; 4];
        for (i, hp) in high_powers.iter_mut().enumerate() {
            *hp = self.highs[i].mag_squared(&self.samples);
        }

        let low = main_freq(&low_powers);
        let high = main_freq(&high_powers);

        match (low, high) {
            (Some(l), Some(h)) => {
                // ensure we don't have any strong harmonics of either of the freqs
                // if we do, it's probably a false positive
                let l2 = self.low_harms[l].mag_squared(&self.samples);

                // harmonic must be less than half
                if l2 > (low_powers[l] / 8.0) {
                    self.no_tone();
                } else {
                    let h2 = self.high_harms[h].mag_squared(&self.samples);

                    if h2 > (high_powers[h] / 8.0) {
                        self.no_tone();
                    } else {
                        self.tone(l, h);
                    }
                }
            }
            _ => {
                self.no_tone();
            }
        }
    }

    fn no_tone(&mut self) {
        if let Some(tone) = self.cur_tone {
            self.old_tone = Some(tone);
            self.cur_tone = None;
            self.state_duration = 0;
        }

        if self.state_duration < STATE_DURATION {
            self.state_duration += 1;

            if self.state_duration == STATE_DURATION
                && self.cur_tone != self.old_old_tone
            {
                // old tone cannot be none because it was set when we first updated our tone to none
                self.tone_changes.push((self.old_tone.unwrap(), State::Off));
                self.old_old_tone = self.cur_tone;
            }
        }
    }

    fn tone(&mut self, low: usize, high: usize) {
        let tone = TONE_TABLE[low][high];

        if self.cur_tone != Some(tone) {
            self.old_tone = self.cur_tone;
            self.cur_tone = Some(tone);
            self.state_duration = 0;
        }

        if self.state_duration < STATE_DURATION {
            self.state_duration += 1;

            if self.state_duration == STATE_DURATION
                && self.cur_tone != self.old_old_tone
            {
                if let Some(old) = self.old_tone {
                    self.tone_changes.push((old, State::Off));
                }

                self.tone_changes.push((tone, State::On));
                self.old_old_tone = self.cur_tone;
            }
        }
    }
}

// gets the dominant frequency in the array, if there is one and it's strong enough
fn main_freq(freqs: &[f32]) -> Option<usize> {
    let mut idx = 0;
    for i in 1..freqs.len() {
        if freqs[i] > freqs[idx] {
            idx = i;
        }
    }

    let mut sum_others = 0.0;
    for (i, v) in freqs.iter().enumerate() {
        if i != idx {
            sum_others += v;
        }
    }

    // main freq must be 8x the sum of the others
    if sum_others * 8.0 < freqs[idx] {
        Some(idx)
    } else {
        None
    }
}
