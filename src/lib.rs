//! Core data structures for local meeting capture and review.

use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Speaker {
    #[default]
    Unidentified,
    Participant(NonZeroU32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpeakerChange {
    pub audio_frame: u64,
    pub speaker: Speaker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutOfOrder {
    pub audio_frame: u64,
    pub latest_frame: u64,
}

/// Tracks speaker labels on the audio clock, regardless of transcription latency.
///
/// A frame is one sample instant across all channels. Before the first change,
/// the speaker is unidentified.
#[derive(Debug, Default)]
pub struct SpeakerTimeline {
    changes: Vec<SpeakerChange>,
    latest_frame: u64,
}

impl SpeakerTimeline {
    pub fn record_change(&mut self, audio_frame: u64, speaker: Speaker) -> Result<(), OutOfOrder> {
        if audio_frame < self.latest_frame {
            return Err(OutOfOrder {
                audio_frame,
                latest_frame: self.latest_frame,
            });
        }
        self.latest_frame = audio_frame;

        // Only one label can start at a frame. The most recent input wins.
        if self
            .changes
            .last()
            .is_some_and(|change| change.audio_frame == audio_frame)
        {
            self.changes.pop();
        }
        if self
            .changes
            .last()
            .map_or(Speaker::Unidentified, |change| change.speaker)
            != speaker
        {
            self.changes.push(SpeakerChange {
                audio_frame,
                speaker,
            });
        }
        Ok(())
    }

    pub fn speaker_at(&self, audio_frame: u64) -> Speaker {
        let index = self
            .changes
            .partition_point(|change| change.audio_frame <= audio_frame);
        self.changes[..index]
            .last()
            .map_or(Speaker::Unidentified, |change| change.speaker)
    }

    pub fn changes(&self) -> &[SpeakerChange] {
        &self.changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: Speaker = Speaker::Participant(NonZeroU32::MIN);
    const SECOND: Speaker = Speaker::Participant(NonZeroU32::new(2).unwrap());

    #[test]
    fn speaker_changes_take_effect_at_the_audio_frame() {
        let mut timeline = SpeakerTimeline::default();
        timeline.record_change(4800, FIRST).unwrap();
        timeline.record_change(9600, SECOND).unwrap();
        timeline
            .record_change(14400, Speaker::Unidentified)
            .unwrap();

        assert_eq!(timeline.speaker_at(4799), Speaker::Unidentified);
        assert_eq!(timeline.speaker_at(4800), FIRST);
        assert_eq!(timeline.speaker_at(9599), FIRST);
        assert_eq!(timeline.speaker_at(9600), SECOND);
        assert_eq!(timeline.speaker_at(14400), Speaker::Unidentified);
    }

    #[test]
    fn repeated_or_same_frame_input_does_not_create_extra_changes() {
        let mut timeline = SpeakerTimeline::default();
        timeline.record_change(0, FIRST).unwrap();
        timeline.record_change(120, FIRST).unwrap();
        timeline.record_change(120, SECOND).unwrap();
        timeline.record_change(120, FIRST).unwrap();

        assert_eq!(timeline.changes().len(), 1);
        assert_eq!(timeline.speaker_at(120), FIRST);
        assert_eq!(
            timeline.record_change(119, SECOND),
            Err(OutOfOrder {
                audio_frame: 119,
                latest_frame: 120
            })
        );
    }
}
