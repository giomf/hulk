use std::{collections::HashSet, iter::Peekable, ops::Range};

use color_eyre::Result;
use context_attribute::context;
use framework::{AdditionalOutput, MainOutput};
use nalgebra::{distance, point, vector, Point2, Vector2};
use ordered_float::NotNan;
use projection::Projection;
use serde::{Deserialize, Serialize};
use types::{
    camera_matrix::CameraMatrix,
    filtered_segments::FilteredSegments,
    image_segments::{EdgeType, Segment},
    line::Line,
    line_data::{ImageLines, LineData, LineDiscardReason},
    ycbcr422_image::YCbCr422Image,
};

use crate::ransac::{Ransac, RansacResult};

#[derive(Deserialize, Serialize)]
pub struct LineDetection {}

#[context]
pub struct CreationContext {}

#[context]
pub struct CycleContext {
    lines_in_image: AdditionalOutput<ImageLines, "lines_in_image">,

    allowed_line_length_in_field:
        Parameter<Range<f32>, "line_detection.$cycler_instance.allowed_line_length_in_field">,
    check_line_distance: Parameter<bool, "line_detection.$cycler_instance.check_line_distance">,
    check_line_length: Parameter<bool, "line_detection.$cycler_instance.check_line_length">,
    check_edge_gradient: Parameter<bool, "line_detection.$cycler_instance.check_edge_gradient">,
    check_line_segments_projection:
        Parameter<bool, "line_detection.$cycler_instance.check_line_segments_projection">,
    gradient_alignment: Parameter<f32, "line_detection.$cycler_instance.gradient_alignment">,
    maximum_distance_to_robot:
        Parameter<f32, "line_detection.$cycler_instance.maximum_distance_to_robot">,
    maximum_fit_distance_in_ground:
        Parameter<f32, "line_detection.$cycler_instance.maximum_fit_distance_in_ground">,
    maximum_gap_on_line: Parameter<f32, "line_detection.$cycler_instance.maximum_gap_on_line">,
    maximum_number_of_lines:
        Parameter<usize, "line_detection.$cycler_instance.maximum_number_of_lines">,
    maximum_projected_segment_length:
        Parameter<f32, "line_detection.$cycler_instance.maximum_projected_segment_length">,
    minimum_number_of_points_on_line:
        Parameter<usize, "line_detection.$cycler_instance.minimum_number_of_points_on_line">,
    maximum_merge_gap_in_pixels:
        Parameter<u16, "line_detection.$cycler_instance.maximum_merge_gap_in_pixels">,

    camera_matrix: RequiredInput<Option<CameraMatrix>, "camera_matrix?">,
    filtered_segments: Input<FilteredSegments, "filtered_segments">,
    image: Input<YCbCr422Image, "image">,
}

#[context]
#[derive(Default)]
pub struct MainOutputs {
    pub line_data: MainOutput<Option<LineData>>,
}

impl LineDetection {
    pub fn new(_context: CreationContext) -> Result<Self> {
        Ok(Self {})
    }

    pub fn cycle(&mut self, mut context: CycleContext) -> Result<MainOutputs> {
        let mut image_lines = ImageLines::default();

        let (line_points, used_vertical_filtered_segments) = filter_segments_for_lines(
            context.camera_matrix,
            context.filtered_segments,
            context.image,
            *context.check_line_segments_projection,
            *context.maximum_projected_segment_length,
            *context.check_edge_gradient,
            *context.gradient_alignment,
            *context.maximum_merge_gap_in_pixels,
        );
        if context.lines_in_image.is_subscribed() {
            image_lines.points = line_points
                .iter()
                .map(|point| context.camera_matrix.ground_to_pixel(*point).unwrap())
                .collect();
        }
        let mut ransac = Ransac::new(line_points);
        let mut lines_in_robot = Vec::new();
        for _ in 0..*context.maximum_number_of_lines {
            if ransac.unused_points.len() < *context.minimum_number_of_points_on_line {
                break;
            }
            let RansacResult {
                line: ransac_line,
                used_points,
            } = ransac.next_line(20, *context.maximum_fit_distance_in_ground);
            let ransac_line =
                ransac_line.expect("Insufficient number of line points. Cannot fit line.");
            if used_points.len() < *context.minimum_number_of_points_on_line {
                image_lines
                    .discarded_lines
                    .push((ransac_line, LineDiscardReason::TooFewPoints));
                break;
            }
            let mut points_with_projection_onto_line: Vec<_> = used_points
                .iter()
                .map(|&point| (point, ransac_line.project_point(point)))
                .collect();
            points_with_projection_onto_line.sort_by_key(|(_point, projected_point)| {
                NotNan::new(projected_point.x).expect("Tried to compare NaN")
            });
            let split_index = (1..points_with_projection_onto_line.len())
                .find(|&index| {
                    distance(
                        &points_with_projection_onto_line[index - 1].1,
                        &points_with_projection_onto_line[index].1,
                    ) > *context.maximum_gap_on_line
                })
                .unwrap_or(points_with_projection_onto_line.len());
            let after_gap = points_with_projection_onto_line.split_off(split_index);
            ransac
                .unused_points
                .extend(after_gap.iter().map(|(point, _projected_point)| point));
            if points_with_projection_onto_line.len() < *context.minimum_number_of_points_on_line {
                // just drop and ignore this line
                image_lines
                    .discarded_lines
                    .push((ransac_line, LineDiscardReason::TooFewPoints));
                continue;
            }

            let Some((start_point_in_image, start_point_in_robot)) =
                points_with_projection_onto_line.first().copied()
            else {
                break;
            };
            let Some((end_point_in_image, end_point_in_robot)) =
                points_with_projection_onto_line.last().copied()
            else {
                break;
            };

            let line_in_robot = Line(start_point_in_robot, end_point_in_robot);
            let line_length_in_robot = line_in_robot.length();
            let is_too_short = *context.check_line_length
                && line_length_in_robot < context.allowed_line_length_in_field.start;
            let is_too_long = *context.check_line_length
                && line_length_in_robot > context.allowed_line_length_in_field.end;
            if is_too_short {
                image_lines
                    .discarded_lines
                    .push((ransac_line, LineDiscardReason::LineTooShort));
                continue;
            }
            if is_too_long {
                image_lines
                    .discarded_lines
                    .push((ransac_line, LineDiscardReason::LineTooLong));
                continue;
            }

            let is_too_far = *context.check_line_distance
                && nalgebra::Normed::norm(&line_in_robot.center().coords)
                    > *context.maximum_distance_to_robot;
            if is_too_far {
                image_lines
                    .discarded_lines
                    .push((ransac_line, LineDiscardReason::TooFarAway));
                continue;
            }

            lines_in_robot.push(line_in_robot);
            if context.lines_in_image.is_subscribed() {
                image_lines
                    .lines
                    .push(Line(start_point_in_image, end_point_in_image));
            }
        }
        let line_data = LineData {
            lines_in_robot,
            used_vertical_filtered_segments,
        };

        context.lines_in_image.fill_if_subscribed(|| {
            for line in image_lines.lines.iter_mut().chain(
                image_lines
                    .discarded_lines
                    .iter_mut()
                    .map(|line| &mut line.0),
            ) {
                line.0 = context.camera_matrix.ground_to_pixel(line.0).unwrap();
                line.1 = context.camera_matrix.ground_to_pixel(line.1).unwrap();
            }
            image_lines
        });

        Ok(MainOutputs {
            line_data: Some(line_data).into(),
        })
    }
}

fn get_gradient(image: &YCbCr422Image, point: Point2<u16>) -> Vector2<f32> {
    if point.x < 1
        || point.y < 1
        || point.x > image.width() as u16 - 2
        || point.y > image.height() as u16 - 2
    {
        return vector![0.0, 0.0];
    }
    let px = point.x as u32;
    let py = point.y as u32;
    // Sobel matrix x (transposed)
    // -1 -2 -1
    //  0  0  0
    //  1  2  1
    let gradient_x = (-1.0 * image.at(px - 1, py - 1).y as f32)
        + (-2.0 * image.at(px, py - 1).y as f32)
        + (-1.0 * image.at(px + 1, py - 1).y as f32)
        + (1.0 * image.at(px - 1, py + 1).y as f32)
        + (2.0 * image.at(px, py + 1).y as f32)
        + (1.0 * image.at(px + 1, py + 1).y as f32);
    // Sobel matrix y (transposed)
    //  1  0 -1
    //  2  0 -2
    //  1  0 -1
    let gradient_y = (1.0 * image.at(px - 1, py - 1).y as f32)
        + (-1.0 * image.at(px + 1, py - 1).y as f32)
        + (2.0 * image.at(px - 1, py).y as f32)
        + (-2.0 * image.at(px + 1, py).y as f32)
        + (1.0 * image.at(px - 1, py + 1).y as f32)
        + (-1.0 * image.at(px + 1, py + 1).y as f32);
    let gradient = vector![gradient_x, gradient_y];
    gradient
        .try_normalize(0.0001)
        .unwrap_or_else(Vector2::zeros)
}

struct SegmentMerger<T: Iterator<Item = Segment>> {
    iterator: Peekable<T>,
    maximum_merge_gap: u16,
}

impl<T> SegmentMerger<T>
where
    T: Iterator<Item = Segment>,
{
    fn new(iterator: T, maximum_merge_gap: u16) -> Self {
        Self {
            iterator: iterator.peekable(),
            maximum_merge_gap,
        }
    }
}

impl<T> Iterator for SegmentMerger<T>
where
    T: Iterator<Item = Segment>,
{
    type Item = Segment;

    fn next(&mut self) -> Option<Self::Item> {
        let mut current = self.iterator.next()?;

        while let Some(next) = self.iterator.peek().copied() {
            if next.start - current.end >= self.maximum_merge_gap {
                break;
            }

            let _ = self.iterator.next();
            current.end = next.end;
            current.end_edge_type = next.end_edge_type;
        }

        Some(current)
    }
}

#[allow(clippy::too_many_arguments)]
fn filter_segments_for_lines(
    camera_matrix: &CameraMatrix,
    filtered_segments: &FilteredSegments,
    image: &YCbCr422Image,
    check_line_segments_projection: bool,
    maximum_projected_segment_length: f32,
    check_edge_gradient: bool,
    gradient_alignment: f32,
    maximum_merge_gap: u16,
) -> (Vec<Point2<f32>>, HashSet<Point2<u16>>) {
    let (line_points, used_vertical_filtered_segments) = filtered_segments
        .scan_grid
        .vertical_scan_lines
        .iter()
        .flat_map(|scan_line| {
            let merged_segments =
                SegmentMerger::new(scan_line.segments.iter().copied(), maximum_merge_gap);

            let scan_line_position = scan_line.position;
            merged_segments.filter_map(move |segment| {
                let is_line_segment = is_line_segment(
                    segment,
                    scan_line_position,
                    image,
                    camera_matrix,
                    check_line_segments_projection,
                    maximum_projected_segment_length,
                    check_edge_gradient,
                    gradient_alignment,
                );
                if is_line_segment {
                    Some((scan_line_position, segment))
                } else {
                    None
                }
            })
        })
        .filter_map(|(scan_line_position, segment)| {
            let center = (segment.start + segment.end) as f32 / 2.0;
            Some((
                camera_matrix
                    .pixel_to_ground(point![scan_line_position as f32, center])
                    .ok()?,
                point![scan_line_position, segment.start],
            ))
        })
        .unzip();
    (line_points, used_vertical_filtered_segments)
}

#[allow(clippy::too_many_arguments)]
fn is_line_segment(
    segment: Segment,
    scan_line_position: u16,
    image: &YCbCr422Image,
    camera_matrix: &CameraMatrix,
    check_line_segments_projection: bool,
    maximum_projected_segment_length: f32,
    check_edge_gradient: bool,
    gradient_alignment: f32,
) -> bool {
    if segment.start_edge_type != EdgeType::Rising || segment.end_edge_type != EdgeType::Falling {
        return false;
    }
    let is_too_long = check_line_segments_projection
        && !is_segment_shorter_than(
            camera_matrix,
            point![scan_line_position as f32, segment.start as f32],
            point![scan_line_position as f32, segment.end as f32],
            maximum_projected_segment_length,
        )
        .unwrap_or(false);
    if is_too_long {
        return false;
    }
    if !check_edge_gradient {
        return true;
    }
    // gradients (approximately) point in opposite directions if their dot product is (close to) -1
    let gradient_at_start = get_gradient(image, point![scan_line_position, segment.start]);
    let gradient_at_end = get_gradient(image, point![scan_line_position, segment.end]);
    gradient_at_start.dot(&gradient_at_end) < gradient_alignment
}

fn is_segment_shorter_than(
    camera_matrix: &CameraMatrix,
    segment_start: Point2<f32>,
    segment_end: Point2<f32>,
    maximum_projected_segment_length: f32,
) -> Option<bool> {
    let start_robot_coordinates = camera_matrix.pixel_to_ground(segment_start).ok()?;
    let end_robot_coordinates = camera_matrix.pixel_to_ground(segment_end).ok()?;
    Some(
        distance(&start_robot_coordinates, &end_robot_coordinates)
            <= maximum_projected_segment_length,
    )
}

#[cfg(test)]
mod tests {
    use nalgebra::{vector, Isometry3, Translation, UnitQuaternion};

    use super::*;

    #[test]
    fn check_fixed_segment_size() {
        let image_size = vector![1.0, 1.0];
        let camera_matrix = CameraMatrix::from_normalized_focal_and_center(
            vector![2.0, 2.0],
            point![1.0, 1.0],
            image_size,
            Isometry3 {
                rotation: UnitQuaternion::from_euler_angles(0.0, std::f32::consts::PI / 4.0, 0.0),
                translation: Translation::from(point![0.0, 0.0, 0.5]),
            },
            Isometry3::identity(),
            Isometry3::identity(),
        );
        let start = point![40.0, 2.0];
        let end = point![40.0, 202.0];
        assert!(!is_segment_shorter_than(&camera_matrix, start, end, 0.3).unwrap_or(false));
        let start2 = point![40.0, 364.0];
        let end2 = point![40.0, 366.0];
        assert!(is_segment_shorter_than(&camera_matrix, start2, end2, 0.3).unwrap_or(false));
    }

    #[test]
    fn gradient_of_zero_image() {
        let image = YCbCr422Image::zero(4, 4);
        let point = point![1, 1];
        assert_eq!(get_gradient(&image, point), vector![0.0, 0.0]);
    }
}
