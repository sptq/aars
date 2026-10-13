use serde::{Deserialize, Serialize};

use crate::synth::Pcm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdMode {
    /// Znak wywoławczy wypowiedziany przez TTS.
    Voice,
    /// Znak wywoławczy nadany telegrafią.
    Cw,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MorseParams {
    pub wpm: u32,
    pub tone_hz: f32,
    pub amplitude: f32,
}

impl Default for MorseParams {
    fn default() -> Self {
        Self { wpm: 20, tone_hz: 800.0, amplitude: 0.5 }
    }
}

const MORSE: &[(char, &str)] = &[
    ('A', ".-"), ('B', "-..."), ('C', "-.-."), ('D', "-.."), ('E', "."),
    ('F', "..-."), ('G', "--."), ('H', "...."), ('I', ".."), ('J', ".---"),
    ('K', "-.-"), ('L', ".-.."), ('M', "--"), ('N', "-."), ('O', "---"),
    ('P', ".--."), ('Q', "--.-"), ('R', ".-."), ('S', "..."), ('T', "-"),
    ('U', "..-"), ('V', "...-"), ('W', ".--"), ('X', "-..-"), ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"), ('1', ".----"), ('2', "..---"), ('3', "...--"),
    ('4', "....-"), ('5', "....."), ('6', "-...."), ('7', "--..."),
    ('8', "---.."), ('9', "----."),
    ('/', "-..-."),
];

/// Generuje znak wywoławczy telegrafią. Długość kropki to standardowe
/// 1200/WPM milisekund, odstęp między elementami kropka, między literami
/// trzy kropki.
pub fn morse_pcm(callsign: &str, params: &MorseParams, sample_rate: u32) -> Pcm {
    let dot_ms = 1200.0 / params.wpm as f64;
    let dot_samples = (dot_ms / 1000.0 * sample_rate as f64).round() as usize;

    let mut samples: Vec<i16> = Vec::new();
    let mut first_letter = true;

    for ch in callsign.to_uppercase().chars() {
        let Some((_, pattern)) = MORSE.iter().find(|(c, _)| *c == ch) else {
            continue;
        };

        if !first_letter {
            // Odstęp międzyliterowy: trzy kropki, z czego jedna już jest po
            // ostatnim elemencie poprzedniej litery.
            samples.extend(std::iter::repeat_n(0i16, dot_samples * 2));
        }
        first_letter = false;

        for (i, element) in pattern.chars().enumerate() {
            if i > 0 {
                samples.extend(std::iter::repeat_n(0i16, dot_samples));
            }
            let length = match element {
                '-' => dot_samples * 3,
                _ => dot_samples,
            };
            append_tone(&mut samples, length, params, sample_rate);
        }
        // Przerwa po literze — dopełnia odstęp międzyliterowy.
        samples.extend(std::iter::repeat_n(0i16, dot_samples));
    }

    Pcm { samples, sample_rate }
}

/// Dokleja ton z łagodnymi zboczami — trzask na włączeniu nośnej brzmi
/// źle i rozmazuje widmo.
fn append_tone(out: &mut Vec<i16>, length: usize, params: &MorseParams, sample_rate: u32) {
    let ramp = (sample_rate as usize / 500).min(length / 2).max(1);
    let peak = params.amplitude.clamp(0.0, 1.0) * 32_767.0;

    for i in 0..length {
        let t = i as f32 / sample_rate as f32;
        let envelope = if i < ramp {
            i as f32 / ramp as f32
        } else if i >= length - ramp {
            (length - i) as f32 / ramp as f32
        } else {
            1.0
        };
        let value = f32::sin(t * 2.0 * std::f32::consts::PI * params.tone_hz) * peak * envelope;
        out.push(value as i16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morse_produces_audio_whose_length_matches_the_callsign() {
        let params = MorseParams::default();
        let short = morse_pcm("E", &params, 48_000);
        let long = morse_pcm("SQ2IPS", &params, 48_000);

        assert!(!short.samples.is_empty());
        assert!(long.samples.len() > short.samples.len() * 5, "dłuższy znak to dłuższe audio");
        assert_eq!(long.sample_rate, 48_000);
    }

    #[test]
    fn morse_timing_follows_wpm() {
        let fast = morse_pcm("SQ2IPS", &MorseParams { wpm: 30, ..Default::default() }, 48_000);
        let slow = morse_pcm("SQ2IPS", &MorseParams { wpm: 15, ..Default::default() }, 48_000);

        let ratio = slow.samples.len() as f64 / fast.samples.len() as f64;
        assert!((ratio - 2.0).abs() < 0.1, "dwukrotnie niższe WPM to dwa razy dłuższe audio, ratio {ratio}");
    }

    #[test]
    fn morse_contains_both_tone_and_silence() {
        let pcm = morse_pcm("A", &MorseParams::default(), 48_000);
        assert!(pcm.samples.iter().any(|s| s.unsigned_abs() > 1000), "brak tonu");
        assert!(pcm.samples.iter().any(|s| *s == 0), "brak przerw między elementami");
    }

    #[test]
    fn unknown_characters_are_skipped_rather_than_panicking() {
        let pcm = morse_pcm("SQ2IPS!@#", &MorseParams::default(), 48_000);
        let plain = morse_pcm("SQ2IPS", &MorseParams::default(), 48_000);
        assert_eq!(pcm.samples.len(), plain.samples.len());
    }

    #[test]
    fn an_empty_callsign_produces_no_audio() {
        assert!(morse_pcm("", &MorseParams::default(), 48_000).samples.is_empty());
    }
}
