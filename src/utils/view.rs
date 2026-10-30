use glam::{DMat3, DVec2};
use smithay::utils::{Coordinate, Logical, Point, Rectangle, Size};

/// Immutable presentation view for a single output.
///
/// A view operation, not a coordinate-frame conversion: both input and
/// output remain Local. Focal-clamping and animation policy belong to the
/// owner that constructs the value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputViewport {
    pub focal: Point<f64, Logical>,
    pub factor: f64,
}

impl OutputViewport {
    /// Viewport that leaves local geometry unchanged.
    pub fn identity() -> Self {
        Self::new((0., 0.).into(), 1.)
    }

    /// Creates a viewport around `focal` with a positive scale factor.
    pub fn new(focal: Point<f64, Logical>, factor: f64) -> Self {
        // Avoid degenerate matrices.
        let factor = factor.max(0.0001);

        Self { focal, factor }
    }

    /// Returns the equivalent 2D affine matrix.
    pub fn to_matrix(&self) -> DMat3 {
        let scale = DVec2::splat(self.factor);
        let focal = self.focal;
        let focal = DVec2::new(focal.x, focal.y);

        DMat3::from_translation(focal) * DMat3::from_scale(scale) * DMat3::from_translation(-focal)
    }

    /// Converts a content-local point to a screen point, applying the viewport.
    pub fn content_to_screen<C: Coordinate, N>(&self, point: Point<C, N>) -> Point<C, N> {
        let (x, y) = (point.x.to_f64(), point.y.to_f64());
        let transformed = self.to_matrix() * DVec2::new(x, y).extend(1.);
        Point::new(C::from_f64(transformed.x), C::from_f64(transformed.y))
    }

    /// Converts a screen point to a content-local point, applying the inverse of the viewport.
    pub fn screen_to_content<C: Coordinate, N>(&self, point: Point<C, N>) -> Point<C, N> {
        let (x, y) = (point.x.to_f64(), point.y.to_f64());
        let transformed = self.to_matrix().inverse() * DVec2::new(x, y).extend(1.);
        Point::new(C::from_f64(transformed.x), C::from_f64(transformed.y))
    }

    /// Returns the axis-aligned bounding box of the screen image of `rect`.
    pub fn content_to_screen_rect<C: Coordinate, N>(
        &self,
        rect: Rectangle<C, N>,
    ) -> Rectangle<C, N> {
        bounding_rect(rect, |point| self.content_to_screen(point))
    }

    /// Returns the axis-aligned bounding box of the content preimage of `rect`.
    pub fn screen_to_content_rect<C: Coordinate, N>(
        &self,
        rect: Rectangle<C, N>,
    ) -> Rectangle<C, N> {
        bounding_rect(rect, |point| self.screen_to_content(point))
    }

    pub fn visible_viewport<C: Coordinate, N>(&self, output_size: Size<C, N>) -> Rectangle<C, N> {
        let screen_rect = Rectangle::from_size(output_size);
        self.screen_to_content_rect(screen_rect)
    }
}

fn bounding_rect<C: Coordinate, N>(
    rect: Rectangle<C, N>,
    map: impl Fn(Point<C, N>) -> Point<C, N>,
) -> Rectangle<C, N> {
    let bottom_right = rect.loc + rect.size;
    let points = [
        rect.loc,
        ((bottom_right.x), (rect.loc.y)).into(),
        ((rect.loc.x), (bottom_right.y)).into(),
        bottom_right,
    ];

    Rectangle::bounding_box(points.map(map))
}

#[cfg(test)]
mod tests {
    use approx::assert_relative_eq;
    use glam::DVec3;
    use smithay::utils::{Logical, Point, Rectangle};

    use super::OutputViewport;

    fn viewport() -> OutputViewport {
        OutputViewport::new((100., 80.).into(), 2.)
    }

    #[test]
    fn identity_is_identity() {
        let point: Point<f64, Logical> = (20., 30.).into();
        assert_eq!(OutputViewport::identity().content_to_screen(point), point);
        assert_eq!(OutputViewport::identity().screen_to_content(point), point);
    }

    #[test]
    fn point_round_trip() {
        let transform = viewport();
        let point: Point<f64, Logical> = (140., 125.).into();
        let round_trip = transform.screen_to_content(transform.content_to_screen(point));
        assert_relative_eq!(round_trip.x, point.x);
        assert_relative_eq!(round_trip.y, point.y);
    }

    #[test]
    fn rectangle_round_trip() {
        let transform = viewport();
        let rect: Rectangle<f64, Logical> = Rectangle::new((50., 40.).into(), (100., 80.).into());
        assert_eq!(
            transform.screen_to_content_rect(transform.content_to_screen_rect(rect)),
            rect
        );
    }

    #[test]
    fn matrix_composition_matches_sequential_application() {
        let inner = OutputViewport::new((100., 80.).into(), 1.5);
        let outer = OutputViewport::new((700., 500.).into(), 2.25);
        let point: Point<f64, Logical> = (350., 275.).into();
        let sequential = outer.content_to_screen(inner.content_to_screen(point));
        let matrix = outer.to_matrix() * inner.to_matrix();
        let composed = matrix * DVec3::new(point.x, point.y, 1.0);

        assert_relative_eq!(composed.x, sequential.x, epsilon = 1e-4);
        assert_relative_eq!(composed.y, sequential.y, epsilon = 1e-4);
    }

    #[test]
    fn rectangle_image_and_preimage_have_expected_bounds() {
        let transform = OutputViewport::new((100., 80.).into(), 2.);
        let rect: Rectangle<f64, Logical> = Rectangle::new((90., 70.).into(), (20., 30.).into());

        assert_eq!(
            transform.content_to_screen_rect(rect),
            Rectangle::new((80., 60.).into(), (40., 60.).into())
        );
        assert_eq!(
            transform.screen_to_content_rect(rect),
            Rectangle::new((95., 75.).into(), (10., 15.).into())
        );
    }

    #[test]
    fn identity_preserves_rectangles_exactly() {
        let rect: Rectangle<f64, Logical> =
            Rectangle::new((-10., 20.).into(), (33.5, 44.25).into());
        let identity = OutputViewport::identity();

        assert_eq!(identity.content_to_screen_rect(rect), rect);
        assert_eq!(identity.screen_to_content_rect(rect), rect);
    }

    #[test]
    fn global_local_viewport_round_trip() {
        // Cross-abstraction: Logical -> Local -> Viewport -> inverse Viewport -> Local -> Logical.
        // This is a round-trip test for the with nonzero and negative output origins.
        let viewport = OutputViewport::new((100., 80.).into(), 2.);
        let cases = [
            // (origin, global point)
            ((40., 25.), (120., 90.)),
            ((-200., -100.), (-80., -30.)),
        ];

        for ((ox, oy), (gx, gy)) in cases {
            let global_origin: Point<f64, Logical> = (ox, oy).into();
            let global: Point<f64, Logical> = (gx, gy).into();

            let local = global - global_origin;
            let round_trip =
                viewport.screen_to_content(viewport.content_to_screen(local)) + global_origin;
            assert_relative_eq!(round_trip.x, global.x, epsilon = 1e-6);
            assert_relative_eq!(round_trip.y, global.y, epsilon = 1e-6);

            // Rectangle variant: translate loc, preserve size across the pipeline.
            let global_rect = Rectangle::new((gx, gy).into(), (800., 600.).into());
            let local_rect = Rectangle::new(global_rect.loc - global_origin, global_rect.size);
            let rt_inner =
                viewport.screen_to_content_rect(viewport.content_to_screen_rect(local_rect));
            let rt_rect = Rectangle::new(rt_inner.loc + global_origin, rt_inner.size);
            assert_relative_eq!(rt_rect.loc.x, global_rect.loc.x, epsilon = 1e-6);
            assert_relative_eq!(rt_rect.loc.y, global_rect.loc.y, epsilon = 1e-6);
            assert_relative_eq!(rt_rect.size.w, global_rect.size.w, epsilon = 1e-6);
            assert_relative_eq!(rt_rect.size.h, global_rect.size.h, epsilon = 1e-6);
        }
    }
}
