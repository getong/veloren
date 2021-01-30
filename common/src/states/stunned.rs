use super::utils::*;
use crate::{
    comp::{CharacterState, PoiseState, StateUpdate},
    states::behavior::{CharacterBehavior, JoinData},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Separated out to condense update portions of character state
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaticData {
    /// How long until state begins to exit
    pub buildup_duration: Duration,
    /// How long the state has until exiting
    pub recover_duration: Duration,
    /// Fraction of normal movement speed allowed during the state
    pub movement_speed: f32,
    /// Poise state (used for determining animation in the client)
    pub poise_state: PoiseState,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Data {
    /// Struct containing data that does not change over the course of the
    /// character state
    pub static_data: StaticData,
    /// Timer for each stage
    pub timer: Duration,
    /// What section the character stage is in
    pub stage_section: StageSection,
    /// Whether the character was wielding or not
    pub was_wielded: bool,
}

impl CharacterBehavior for Data {
    fn behavior(&self, data: &JoinData) -> StateUpdate {
        let mut update = StateUpdate::from(data);

        handle_move(data, &mut update, self.static_data.movement_speed);

        match self.stage_section {
            StageSection::Buildup => {
                if self.timer < self.static_data.buildup_duration {
                    // Build up
                    update.character = CharacterState::Stunned(Data {
                        timer: self
                            .timer
                            .checked_add(Duration::from_secs_f32(data.dt.0))
                            .unwrap_or_default(),
                        ..*self
                    });
                } else {
                    // Transitions to recovery section of stage
                    update.character = CharacterState::Stunned(Data {
                        timer: Duration::default(),
                        stage_section: StageSection::Recover,
                        ..*self
                    });
                }
            },
            StageSection::Recover => {
                if self.timer < self.static_data.recover_duration {
                    // Recovery
                    update.character = CharacterState::Stunned(Data {
                        timer: self
                            .timer
                            .checked_add(Duration::from_secs_f32(data.dt.0))
                            .unwrap_or_default(),
                        ..*self
                    });
                } else {
                    // Done
                    if self.was_wielded {
                        update.character = CharacterState::Wielding;
                    } else {
                        update.character = CharacterState::Idle;
                    }
                }
            },
            _ => {
                // If it somehow ends up in an incorrect stage section
                if self.was_wielded {
                    update.character = CharacterState::Wielding;
                } else {
                    update.character = CharacterState::Idle;
                }
            },
        }

        update
    }
}