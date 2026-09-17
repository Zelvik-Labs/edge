//! Unary event batch + sequence (StreamEvents streaming can wrap this later).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBatchError {
    Duplicate { sequence: u64 },
    Gap { expected: u64, actual: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAck {
    pub ack_sequence: u64,
    pub idempotent_replay: bool,
}

#[derive(Debug, Default)]
pub struct EventBatchIngress {
    next_sequence: u64,
}

impl EventBatchIngress {
    pub fn ingest(&mut self, sequence: u64, _payload: &[u8]) -> Result<EventAck, EventBatchError> {
        if self.next_sequence == 0 && sequence == 0 {
            self.next_sequence = 1;
            return Ok(EventAck {
                ack_sequence: 0,
                idempotent_replay: false,
            });
        }
        if sequence + 1 == self.next_sequence {
            return Ok(EventAck {
                ack_sequence: sequence,
                idempotent_replay: true,
            });
        }
        if sequence < self.next_sequence {
            return Err(EventBatchError::Duplicate { sequence });
        }
        if sequence != self.next_sequence {
            return Err(EventBatchError::Gap {
                expected: self.next_sequence,
                actual: sequence,
            });
        }
        self.next_sequence = sequence + 1;
        Ok(EventAck {
            ack_sequence: sequence,
            idempotent_replay: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_batches_ack_sequence() {
        let mut q = EventBatchIngress::default();
        assert_eq!(q.ingest(0, b"a").unwrap().ack_sequence, 0);
        assert_eq!(q.ingest(1, b"b").unwrap().ack_sequence, 1);
    }

    #[test]
    fn duplicate_sequence_is_idempotent() {
        let mut q = EventBatchIngress::default();
        q.ingest(0, b"a").unwrap();
        let replay = q.ingest(0, b"a").unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.ack_sequence, 0);
    }

    #[test]
    fn gap_is_rejected() {
        let mut q = EventBatchIngress::default();
        q.ingest(0, b"a").unwrap();
        assert!(matches!(
            q.ingest(3, b"x"),
            Err(EventBatchError::Gap {
                expected: 1,
                actual: 3
            })
        ));
    }
}
