//! "Me or someone else", live, from how loud a sentence was.
//!
//! The pendant hangs on the wearer: their voice reaches the microphone from a
//! few centimetres and everyone else's from a metre or more, so the sentence
//! levels in a meeting fall into two clumps. Splitting them is a 1-D two-means;
//! a sentence the wearer corrects on the page is pinned to its side, so the
//! split follows the corrections. It is a first guess the page lets you fix,
//! not diarization — that comes after the meeting.

use std::collections::VecDeque;

/// Until there are enough sentences to see two clumps.
const FALLBACK_DB: f32 = -28.0;
/// Fewer sentences than this and the clumps mean nothing yet.
const MIN_SAMPLES: usize = 6;
/// Clumps closer than this are one voice (or one distance), not two.
const MIN_SPREAD_DB: f32 = 6.0;
const WINDOW: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct SpeakerModel {
    /// (level in dBFS, the wearer's correction if any)
    samples: VecDeque<(f32, Option<bool>)>,
}

impl SpeakerModel {
    pub fn observe(&mut self, rms_db: f32) {
        self.push((rms_db, None));
    }

    /// The wearer said this level was (or was not) them.
    pub fn correct(&mut self, rms_db: f32, is_me: bool) {
        self.push((rms_db, Some(is_me)));
    }

    fn push(&mut self, s: (f32, Option<bool>)) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    pub fn is_me(&self, rms_db: f32) -> bool {
        rms_db >= self.threshold()
    }

    pub fn threshold(&self) -> f32 {
        if self.samples.len() < MIN_SAMPLES {
            return FALLBACK_DB;
        }
        let mut t = self.samples.iter().map(|s| s.0).sum::<f32>() / self.samples.len() as f32;
        for _ in 0..20 {
            let (mut hi, mut nh, mut lo, mut nl) = (0.0f32, 0u32, 0.0f32, 0u32);
            for &(v, fixed) in &self.samples {
                if fixed.unwrap_or(v >= t) {
                    hi += v;
                    nh += 1;
                } else {
                    lo += v;
                    nl += 1;
                }
            }
            if nh == 0 || nl == 0 {
                return FALLBACK_DB;
            }
            let (mh, ml) = (hi / nh as f32, lo / nl as f32);
            if mh - ml < MIN_SPREAD_DB {
                return FALLBACK_DB;
            }
            let next = (mh + ml) / 2.0;
            if (next - t).abs() < 0.01 {
                return next;
            }
            t = next;
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_until_two_clumps_show() {
        let mut m = SpeakerModel::default();
        assert!(m.is_me(-20.0) && !m.is_me(-40.0));
        for _ in 0..10 {
            m.observe(-30.0);
        }
        assert_eq!(m.threshold(), FALLBACK_DB);
    }

    #[test]
    fn splits_between_near_and_far_voices() {
        let mut m = SpeakerModel::default();
        for i in 0..10 {
            m.observe(-18.0 - (i % 3) as f32);
            m.observe(-38.0 + (i % 4) as f32);
        }
        let t = m.threshold();
        assert!(t > -30.0 && t < -26.0, "{t}");
        assert!(m.is_me(-21.0));
        assert!(!m.is_me(-34.0));
    }

    #[test]
    fn corrections_move_the_split() {
        let mut m = SpeakerModel::default();
        for _ in 0..10 {
            m.observe(-18.0);
            m.observe(-38.0);
        }
        assert!(!m.is_me(-29.0));
        // The wearer, speaking softly, says these were them.
        for _ in 0..8 {
            m.correct(-30.0, true);
        }
        assert!(m.is_me(-29.0));
    }
}
