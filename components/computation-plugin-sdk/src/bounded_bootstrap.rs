// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{
    BootstrapCompletion, ChangeEnvelope, GraphProducerProgress, OutputCursor, StreamId,
};

/// Immutable overlap boundaries for one volatile initialization. Install them
/// only on completion and discard them on restart. These are neither a live
/// delivery ledger nor a promise that the producer can replay.
#[derive(Default)]
pub struct BoundedBootstrapCuts(BTreeMap<StreamId, OutputCursor>);

impl TryFrom<BootstrapCompletion> for BoundedBootstrapCuts {
    type Error = anyhow::Error;

    fn try_from(completion: BootstrapCompletion) -> Result<Self> {
        ensure!(
            completion.state.is_none(),
            "bounded bootstrap cannot accept connector handover state"
        );
        ensure!(
            completion.watermarks.len() <= crate::abi::bootstrap::MAX_WATERMARKS,
            "native bootstrap watermark limit exceeded"
        );
        let mut cuts = BTreeMap::new();
        for watermark in completion.watermarks {
            watermark.validate()?;
            let producer = watermark.producer.context(
                "bounded bootstrap requires an output cursor, not a source-driver watermark",
            )?;
            let cut = OutputCursor::try_new(producer, watermark.sequence)?;
            ensure!(
                cuts.insert(watermark.stream, cut).is_none(),
                "ambiguous bootstrap cuts on one input stream"
            );
        }
        Ok(Self(cuts))
    }
}

impl BoundedBootstrapCuts {
    /// Compare the *immediate producer's logical position*, never a snapshot
    /// ordinal, ancestor annotation or transport sequence. An uncut stream is
    /// ordinary live input; an unidentifiable or changed epoch on a cut stream
    /// cannot safely continue that baseline.
    pub fn covers(&self, envelope: &ChangeEnvelope) -> Result<bool> {
        let Some(cut) = self.0.get(envelope.system().stream()) else {
            return Ok(false);
        };
        let position = GraphProducerProgress::from_envelope(envelope)?
            .context("live input on a bootstrap cut stream lacks its producer's progress")?;
        ensure!(
            position.identity() == cut.producer(),
            "live input producer or epoch differs from the bootstrap cut; rebuild required"
        );
        Ok(position.sequence() <= cut.sequence())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{
        BootstrapWatermark, ComponentId, GraphChangeCodec, GraphProducerIdentity,
    };

    #[test]
    fn only_output_cuts_are_comparable_and_only_on_their_streams() -> Result<()> {
        let stream = StreamId::try_new("upstream/out")?;
        let producer = GraphProducerIdentity::volatile(
            "instance".into(),
            "graph".into(),
            ComponentId::try_new("30361edc-3edd-4da2-87a6-f229a3ac9baf")?,
            stream.clone(),
        )?;
        let watermark = BootstrapWatermark::from_output(&OutputCursor::try_new(producer, 4)?);
        let completion = |watermarks| BootstrapCompletion {
            watermarks,
            state: None,
        };
        let cuts = BoundedBootstrapCuts::try_from(completion(vec![watermark.clone()]))?;
        let unmarked = |stream| {
            GraphChangeCodec::encode_changes(&[], stream, 1, None).map_err(anyhow::Error::from)
        };
        assert!(!cuts.covers(&unmarked(StreamId::try_new("other/out")?)?)?);
        assert!(cuts.covers(&unmarked(stream)?).is_err());
        assert!(BoundedBootstrapCuts::try_from(completion(vec![
            watermark.clone(),
            watermark.clone()
        ]))
        .is_err());
        assert!(BoundedBootstrapCuts::try_from(BootstrapCompletion {
            state: Some(bytes::Bytes::from_static(b"intent")),
            ..Default::default()
        })
        .is_err());
        assert!(
            BoundedBootstrapCuts::try_from(completion(vec![BootstrapWatermark {
                producer: None,
                ..watermark
            }]))
            .is_err()
        );
        Ok(())
    }
}
