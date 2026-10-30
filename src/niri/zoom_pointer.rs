//! Pure pointer reconciliation. Zoom effects describe old presentation, not current drivers.

use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle};

use super::PointerProvenance;
use crate::layout::zoom::ZoomPointerChange;
use crate::utils::view::OutputViewport;

/// Include every existing output, even an owner with no pending effect or mapped geometry.
pub(super) struct OutputSnapshot {
    pub output: Output,
    pub geometry: Option<Rectangle<f64, Logical>>,
    pub view: OutputViewport,
    pub pending: Option<ZoomPointerChange>,
}

pub(super) struct SelectedEffect {
    pub output: Output,
    /// Selection still matters to interruption policy if the output has no mapped geometry.
    pub movement: Option<PointerMovement>,
}

pub(super) struct PointerMovement {
    pub geometry: Rectangle<f64, Logical>,
    pub location: Point<f64, Logical>,
}

/// `current` is the compositor's current spatial ownership, independent of snapshot ordering.
/// `prior` is validated only by the presence of its output in `outputs`; actual location
/// writers maintain provenance, so the current position is not a proxy for prior validity.
pub(super) fn plan_reprojection(
    position: Point<f64, Logical>,
    prior: Option<&PointerProvenance>,
    current: Option<PointerProvenance>,
    outputs: &[OutputSnapshot],
) -> (Option<PointerProvenance>, Option<SelectedEffect>) {
    let prior_owner = prior.and_then(|prior| {
        outputs
            .iter()
            .find(|output| output.output == prior.output)
            .map(|owner| (prior, owner))
    });
    let provenance = match prior_owner {
        Some((prior, owner))
            if owner.geometry != Some(prior.geometry) && owner.pending.is_none() =>
        {
            // Without an original-owner effect, future reprojection must use current placement.
            current
        }
        Some((prior, _)) => Some(prior.clone()),
        None => current,
    };

    let selected = if let Some(provenance) = &provenance {
        // A known owner without its own effect prevents another output from moving the seat.
        outputs
            .iter()
            .find(|output| output.output == provenance.output)
            .and_then(|output| {
                output
                    .pending
                    .map(|change| (output, change.old_view, provenance.geometry.loc))
            })
    } else {
        // Unknown ownership permits only a unique old resize-bounds match at the current origin.
        let mut candidates = outputs.iter().filter_map(|output| {
            let change = output.pending?;
            let geometry = output.geometry?;
            (change.old_size != geometry.size
                && Rectangle::from_size(change.old_size).contains(position - geometry.loc))
            .then_some((output, change.old_view, geometry.loc))
        });
        let candidate = candidates.next();
        if candidates.next().is_none() {
            candidate
        } else {
            None
        }
    };

    let selected = selected.map(|(output, old, old_origin)| SelectedEffect {
        output: output.output.clone(),
        movement: output.geometry.map(|geometry| {
            let mut screen = old.content_to_screen(position - old_origin);
            // Preserve valid fractional edges; only out-of-bounds components use legal fallback.
            if screen.x < 0. || screen.x >= geometry.size.w {
                screen.x = screen.x.clamp(0., geometry.size.w - 1.);
            }
            if screen.y < 0. || screen.y >= geometry.size.h {
                screen.y = screen.y.clamp(0., geometry.size.h - 1.);
            }
            let local = output
                .view
                .screen_to_content(screen)
                .constrain(Rectangle::from_size(geometry.size));
            PointerMovement {
                geometry,
                location: local + geometry.loc,
            }
        }),
    });

    (provenance, selected)
}
