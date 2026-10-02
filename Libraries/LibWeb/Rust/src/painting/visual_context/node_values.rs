/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

use super::{ClipData, EffectsData, PerspectiveData, TransformData, TransformDataRole};
use crate::css::computed_value_types::{ComputedClipEdge, ComputedStyleValueHandle};
use crate::css::computed_value_views::{ComputedValuesView, LengthPercentageRef};
use crate::css::css_enums;
use crate::css::css_pixels::CssPixels;
use crate::css::css_pixels::{CssPixelPoint, CssPixelRect};
use crate::css::style_value::StyleValueData;
use crate::layout::node_data::NodeSlotId;
use crate::layout::node_facts;
use crate::painting::border_radii::BorderRadii;
use crate::painting::display_list::device_pixels::DevicePixelConverter;
use crate::painting::host::FfiVisualContextTreeInputs;
use crate::painting::node_painting;
use crate::painting::paint_read::PaintRead;
use crate::painting::paintable_geometry;
use crate::painting::paintable_rows::PaintableRowsRead;
use crate::painting::style_queries;
use libgfx_rust::{
    AffineTransform, CompositingAndBlendingOperator, CornerRadii, FloatPoint, affine_to_matrix, perspective_matrix,
    scale_matrix_for_device_pixels, translation_matrix,
};

pub(crate) fn visual_viewport_transform_data(inputs: &FfiVisualContextTreeInputs) -> TransformData {
    let scaled_offset_x = inputs.visual_viewport_offset_x * inputs.visual_viewport_scale;
    let scaled_offset_y = inputs.visual_viewport_offset_y * inputs.visual_viewport_scale;
    let scale = inputs.visual_viewport_scale as f32;
    let translation_x = (-scaled_offset_x) as f32 + 0.0;
    let translation_y = (-scaled_offset_y) as f32 + 0.0;
    let visual_viewport_affine = AffineTransform {
        values: [scale, 0.0, 0.0, scale, translation_x, translation_y],
    };
    TransformData {
        matrix: scale_matrix_for_device_pixels(
            affine_to_matrix(visual_viewport_affine),
            inputs.device_pixels_per_css_pixel as f32,
        ),
        origin: FloatPoint::default(),
        sorting_context_root_index: None,
        flattens_inherited_transform: false,
        role: TransformDataRole::CssTransform,
        synthetic_plane: false,
        establishes_sorting_context: false,
    }
}

pub(crate) fn transform_reference_box(
    style: ComputedValuesView<'_>,
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
) -> CssPixelRect {
    use css_enums::transform_box::{BORDER_BOX, CONTENT_BOX, FILL_BOX, STROKE_BOX, VIEW_BOX};
    let mut transform_box = style.transform().transform_box;
    if layout_arena.node_kind_if_live(slot).is_some_and(node_painting::is_svg) {
        transform_box = match transform_box {
            CONTENT_BOX => FILL_BOX,
            BORDER_BOX => STROKE_BOX,
            other => other,
        };
    } else {
        transform_box = match transform_box {
            FILL_BOX => CONTENT_BOX,
            STROKE_BOX | VIEW_BOX => BORDER_BOX,
            other => other,
        };
    }
    match transform_box {
        CONTENT_BOX | FILL_BOX => paintable_geometry::absolute_rect(layout_arena, slot),
        VIEW_BOX => crate::painting::svg_viewport::nearest_svg_viewport_user_rect(layout_arena, slot)
            .map(|rect| {
                CssPixelRect::new(
                    CssPixels::default(),
                    CssPixels::default(),
                    CssPixels::nearest_value_for(rect.width as f64),
                    CssPixels::nearest_value_for(rect.height as f64),
                )
            })
            .unwrap_or_else(|| paintable_geometry::absolute_border_box_rect(layout_arena, slot)),
        _ => paintable_geometry::absolute_border_box_rect(layout_arena, slot),
    }
}

fn resolved_translate_axis_px(px: f32, percentage: &ComputedStyleValueHandle, reference: CssPixels) -> f32 {
    let Some(length_percentage) = percentage.length_percentage() else {
        return px;
    };
    if length_percentage.is_calculated() {
        return CssPixels::nearest_value_for(crate::css::computed_value_views::resolve_calc_to_px_without_rounding(
            length_percentage.calculated_pointer(),
            reference,
        ))
        .to_float();
    }
    CssPixels::nearest_value_for(reference.to_double() * length_percentage.as_fraction()).to_float()
}

fn resolved_transform_to_matrix(
    entry: &crate::css::computed_value_types::ComputedResolvedTransform,
    reference_width: CssPixels,
    reference_height: CssPixels,
) -> libgfx_rust::FloatMatrix4x4 {
    if entry.is_translate {
        return translation_matrix(
            resolved_translate_axis_px(entry.x_px, &entry.x_percentage, reference_width),
            resolved_translate_axis_px(entry.y_px, &entry.y_percentage, reference_height),
            entry.z_px,
        );
    }
    let mut elements = [[0.0f32; 4]; 4];
    for (index, value) in entry.matrix.into_iter().enumerate() {
        elements[index / 4][index % 4] = value;
    }
    libgfx_rust::FloatMatrix4x4 { elements }
}

pub(crate) fn multiply_transform_functions(
    mut matrix: libgfx_rust::FloatMatrix4x4,
    entries: &[crate::css::computed_value_types::ComputedResolvedTransform],
    reference_box: CssPixelRect,
) -> libgfx_rust::FloatMatrix4x4 {
    for entry in entries {
        matrix = matrix.multiplied(resolved_transform_to_matrix(
            entry,
            reference_box.width,
            reference_box.height,
        ));
    }
    matrix
}

/// The offset distance in pixels, where 100% is the given path length. Unlike LengthPercentageRef::to_px(), this
/// keeps the fraction of a pixel that a percentage of a path length resolves to.
fn offset_distance_px(offset_distance: &ComputedStyleValueHandle, path_length: f32) -> f32 {
    let Some(distance) = offset_distance.length_percentage() else {
        return 0.0;
    };
    if distance.is_calculated() {
        return crate::css::computed_value_views::resolve_calc_to_px_without_rounding(
            distance.calculated_pointer(),
            CssPixels::nearest_value_for_f32(path_length),
        ) as f32;
    }
    if distance.contains_percentage() {
        return (f64::from(path_length) * distance.as_fraction()) as f32;
    }
    distance.absolute_length_to_px().to_float()
}

// https://drafts.csswg.org/motion-1/#ray-function
/// The offset position and path direction, in radians, of a ray() offset path.
// FIXME: Clamp the distance for contain, so that the box lies entirely within the path.
fn position_along_ray(
    ray: &[crate::css::style_value::RetainedStyleValueData],
    containing_block_rect: CssPixelRect,
    offset_starting_position: Option<CssPixelPoint>,
    offset_distance: &ComputedStyleValueHandle,
) -> Option<([f32; 2], f32)> {
    use crate::css::calc::ANGLE_UNIT_CANONICAL_RATIOS;
    use css_enums::keyword;

    let StyleValueData::Angle { value, unit } = ray.first()?.data() else {
        return None;
    };
    let angle = (value * ANGLE_UNIT_CANONICAL_RATIOS[*unit as usize]).to_radians() as f32;
    let mut size = keyword::CLOSEST_SIDE;
    let mut position = None;
    for (index, part) in ray.iter().enumerate().skip(1) {
        match part.data() {
            StyleValueData::Keyword { keyword } if *keyword != keyword::CONTAIN => size = *keyword,
            StyleValueData::CustomIdent { .. } => position = ray.get(index + 1).map(|position| position.data()),
            _ => {}
        }
    }

    // The ray starts at its "at <position>", or else at the offset starting position, or else at the center of the
    // containing block.
    let start = match (position, offset_starting_position) {
        (Some(position), _) => super::basic_shapes::position_resolved(Some(position), containing_block_rect),
        (None, Some(offset_starting_position)) => offset_starting_position,
        (None, None) => super::basic_shapes::position_resolved(None, containing_block_rect),
    };
    let [start_x, start_y] = [start.x.to_float(), start.y.to_float()];
    let left = containing_block_rect.x.to_float();
    let top = containing_block_rect.y.to_float();
    let right = left + containing_block_rect.width.to_float();
    let bottom = top + containing_block_rect.height.to_float();

    // An angle of 0deg points up, and positive angles increase clockwise.
    let (direction_x, direction_y) = (angle.sin(), -angle.cos());

    // https://drafts.csswg.org/motion-1/#typedef-ray-size
    let side_distances = [start_x - left, right - start_x, start_y - top, bottom - start_y].map(f32::abs);
    let corner_distances = [(left, top), (right, top), (right, bottom), (left, bottom)]
        .map(|(corner_x, corner_y)| (corner_x - start_x).hypot(corner_y - start_y));
    let ray_length = match size {
        keyword::FARTHEST_SIDE => side_distances.into_iter().fold(0.0, f32::max),
        keyword::CLOSEST_CORNER => corner_distances.into_iter().fold(f32::INFINITY, f32::min),
        keyword::FARTHEST_CORNER => corner_distances.into_iter().fold(0.0, f32::max),
        // The distance between the ray's starting point and the point where it intersects the containing block's
        // boundary. If the starting point is outside the boundary, the length is 0.
        keyword::SIDES => {
            if start_x < left || start_x > right || start_y < top || start_y > bottom {
                0.0
            } else {
                let along = |direction: f32, low: f32, high: f32, start: f32| {
                    if direction > 0.0 {
                        (high - start) / direction
                    } else if direction < 0.0 {
                        (low - start) / direction
                    } else {
                        f32::INFINITY
                    }
                };
                along(direction_x, left, right, start_x).min(along(direction_y, top, bottom, start_y))
            }
        }
        _ => side_distances.into_iter().fold(f32::INFINITY, f32::min),
    };

    // References to <angle> offset paths without contain are unbounded rays: the used offset distance is the offset
    // distance.
    let distance = offset_distance_px(offset_distance, ray_length);
    Some((
        [start_x + distance * direction_x, start_y + distance * direction_y],
        angle - std::f32::consts::FRAC_PI_2,
    ))
}

// https://drafts.csswg.org/motion-1/#calculating-the-computed-distance-along-a-path
/// The offset position and path direction, in radians, at the used offset distance along a path.
fn position_along_path(
    path: &libgfx_rust::path::OwnedPath,
    is_closed_loop: bool,
    offset_distance: &ComputedStyleValueHandle,
) -> Option<([f32; 2], f32)> {
    // 1. Let the total length be the total length of offset path with all sub-paths.
    let total_length = path.length();
    // 2. Convert offset distance to pixels, with 100% being converted to total length.
    let distance = offset_distance_px(offset_distance, total_length);
    let used_distance = if is_closed_loop {
        // Let used offset distance be equal to offset distance modulo the total length of the path. If the total
        // length of the path is 0, used offset distance is also 0.
        if total_length == 0.0 {
            0.0
        } else {
            distance.rem_euclid(total_length)
        }
    } else {
        // Let used offset distance be equal to offset distance clamped by 0 and the total length of the path.
        distance.clamp(0.0, total_length)
    };
    let (position, [tangent_x, tangent_y]) = path.position_and_tangent_at(used_distance)?;
    Some((position, tangent_y.atan2(tangent_x)))
}

// https://drafts.csswg.org/motion-1/#offset-transform
/// The offset transform, relative to the transform origin, of a box with an offset path. It translates the box so
/// that its anchor point lies on the offset position, and rotates it about that point by offset-rotate.
// FIXME: Place SVG elements on their offset path, whose reference boxes and containing blocks come from SVG layout.
fn compute_offset_transform(
    layout_arena: &impl PaintRead,
    node: NodeSlotId,
    style: ComputedValuesView<'_>,
    reference_box: CssPixelRect,
    origin_x: CssPixels,
    origin_y: CssPixels,
) -> Option<libgfx_rust::FloatMatrix4x4> {
    use crate::css::calc::ANGLE_UNIT_CANONICAL_RATIOS;
    use css_enums::keyword;

    let transform_values = style.transform();
    let offset_path = style_queries::handle_value(&transform_values.offset_path)?;
    let (path, coord_box) = match offset_path {
        StyleValueData::ValueList { values, .. } => match values.as_slice() {
            [path, coord_box] => (Some(path.data()), Some(coord_box.data())),
            _ => return None,
        },
        StyleValueData::Keyword { .. } => (None, Some(offset_path)),
        path => (Some(path), None),
    };

    // The <coord-box> of the containing block provides the reference box for the path. If it is omitted, it defaults
    // to border-box.
    let containing_block = layout_arena.node_containing_block_if_live(node)?;
    let containing_block_rect = match coord_box {
        Some(StyleValueData::Keyword { keyword }) if matches!(*keyword, keyword::CONTENT_BOX | keyword::FILL_BOX) => {
            paintable_geometry::absolute_rect(layout_arena, containing_block)
        }
        Some(StyleValueData::Keyword { keyword }) if *keyword == keyword::PADDING_BOX => {
            paintable_geometry::absolute_padding_box_rect(layout_arena, containing_block)
        }
        _ => paintable_geometry::absolute_border_box_rect(layout_arena, containing_block),
    };

    // https://drafts.csswg.org/motion-1/#offset-position-property
    let offset_starting_position = match style_queries::handle_value(&transform_values.offset_position) {
        // normal: The element does not have an offset starting position.
        None => None,
        // auto: The offset starting position is the top-left corner of the box.
        Some(StyleValueData::Keyword { keyword }) if *keyword == keyword::AUTO => {
            let border_box = paintable_geometry::absolute_border_box_rect(layout_arena, node);
            Some(CssPixelPoint::new(border_box.x, border_box.y))
        }
        // <position>: The result of using the <position> to position a 0x0 object area within the box's containing
        // block.
        Some(position) => Some(super::basic_shapes::position_resolved(
            Some(position),
            containing_block_rect,
        )),
    };

    let ([position_x, position_y], path_direction) = match path {
        Some(StyleValueData::Function { name, value }) if name.units().iter().copied().eq("ray".encode_utf16()) => {
            let StyleValueData::ValueList { values, .. } = value.optional_data()? else {
                return None;
            };
            position_along_ray(
                values.as_slice(),
                containing_block_rect,
                offset_starting_position,
                &transform_values.offset_distance,
            )?
        }
        Some(shape @ StyleValueData::BasicShape { .. }) => {
            let shape = shape.basic_shape()?;
            // A path() without a <coord-box> lays its coordinates out from the box's own position.
            let path_box = if shape.kind == super::basic_shapes::basic_shape_kind::PATH && coord_box.is_none() {
                let border_box = paintable_geometry::absolute_border_box_rect(layout_arena, node);
                CssPixelRect::new(
                    border_box.x,
                    border_box.y,
                    CssPixels::from_raw(0),
                    CssPixels::from_raw(0),
                )
            } else {
                containing_block_rect
            };
            let start_in_path_box = offset_starting_position
                .map(|position| CssPixelPoint::new(position.x - path_box.x, position.y - path_box.y));
            let (shape_path, is_closed_loop) =
                super::basic_shapes::basic_shape_equivalent_path(shape, containing_block_rect, start_in_path_box);
            let ([x, y], direction) =
                position_along_path(&shape_path, is_closed_loop, &transform_values.offset_distance)?;
            ([path_box.x.to_float() + x, path_box.y.to_float() + y], direction)
        }
        // If <offset-path> is omitted, it defaults to inset(0 round X), where X is the value of border-radius on the
        // element that establishes the containing block for this element.
        // FIXME: Round the corners by the containing block's border-radius.
        None => {
            let [left, top] = [containing_block_rect.x.to_float(), containing_block_rect.y.to_float()];
            let [right, bottom] = [
                left + containing_block_rect.width.to_float(),
                top + containing_block_rect.height.to_float(),
            ];
            let mut builder = libgfx_rust::path::PathBuilder::new();
            builder.move_to(left, top);
            builder.line_to(right, top);
            builder.line_to(right, bottom);
            builder.line_to(left, bottom);
            builder.close();
            position_along_path(&builder.build(), true, &transform_values.offset_distance)?
        }
        // FIXME: Follow a <url> to its SVG shape element's equivalent path.
        Some(_) => return None,
    };

    // https://drafts.csswg.org/motion-1/#offset-rotate-property
    let angle_in_radians = |angle: &StyleValueData| match angle {
        StyleValueData::Angle { value, unit } => {
            Some((value * ANGLE_UNIT_CANONICAL_RATIOS[*unit as usize]).to_radians() as f32)
        }
        _ => None,
    };
    let rotation = match style_queries::handle_value(&transform_values.offset_rotate) {
        // auto: A rotation equal to the offset path's direction at the offset position.
        None => path_direction,
        Some(StyleValueData::Keyword { .. }) => path_direction,
        // auto <angle>: The angle is added to the rotation component.
        Some(StyleValueData::ValueList { values, .. }) => {
            path_direction
                + values
                    .as_slice()
                    .last()
                    .and_then(|angle| angle_in_radians(angle.data()))?
        }
        // <angle>: A rotation of the specified angle.
        Some(angle) => angle_in_radians(angle)?,
    };

    // https://drafts.csswg.org/motion-1/#offset-anchor-property
    let [origin_x, origin_y] = [origin_x.to_float(), origin_y.to_float()];
    let [anchor_x, anchor_y] = match style_queries::handle_value(&transform_values.offset_anchor) {
        // auto: The anchor point is the same as the point indicated by transform-origin.
        None => [origin_x, origin_y],
        // <position>: The result of resolving the <position> against the element's reference box.
        Some(position) => {
            let anchor = super::basic_shapes::position_resolved(
                Some(position),
                CssPixelRect::new(
                    CssPixels::from_raw(0),
                    CssPixels::from_raw(0),
                    reference_box.width,
                    reference_box.height,
                ),
            );
            [anchor.x.to_float(), anchor.y.to_float()]
        }
    };

    // https://drafts.csswg.org/motion-1/#calculating-the-offset-transform
    // The offset transform is a 2d transform, a translation followed by a rotation:
    // 1. Translate the element by the (X, Y) that aligns its anchor point with its offset position.
    // 2. Rotate the element by the angle specified by offset-rotate.
    // NB: The matrix applies about the transform origin, so the rotation happens about the anchor point by
    //     translating it to the transform origin first.
    let offset_x = position_x - reference_box.x.to_float();
    let offset_y = position_y - reference_box.y.to_float();
    Some(
        translation_matrix(offset_x - origin_x, offset_y - origin_y, 0.0)
            .multiplied(libgfx_rust::rotation_matrix([0.0, 0.0, 1.0], rotation))
            .multiplied(translation_matrix(origin_x - anchor_x, origin_y - anchor_y, 0.0)),
    )
}

// https://drafts.csswg.org/css-transforms-2/#ctm
pub(crate) fn compute_transform(
    layout_arena: &impl PaintRead,
    node: NodeSlotId,
    pixel_ratio: f64,
) -> Option<(TransformData, bool)> {
    let style = layout_arena.node_style_if_live(node)?;
    let node_kind = layout_arena.node_kind_if_live(node)?;

    let additional_element_transform = if style_queries::kind_is_svg_element_box(node_kind) {
        crate::painting::paintable_geometry::committed_svg_additional_element_transform(layout_arena, node)
            .map(Into::into)
    } else {
        None
    };

    let transform_values = style.transform();
    let style_has_transform =
        transform_values.resolved_transforms.length != 0 || !transform_values.offset_path.pointer.is_null();
    let has_transform_node_input = style_has_transform
        || additional_element_transform.is_some()
        || style_queries::will_change_promotes_transform_node(style);
    if !has_transform_node_input || !style_queries::is_transformable(layout_arena, node) {
        return None;
    }

    // The transformation matrix is computed from the transform, transform-origin, translate, rotate, scale, and
    // offset properties as follows:
    let reference_box = transform_reference_box(style, layout_arena, node);
    let origin_lp = |handle: &ComputedStyleValueHandle| {
        handle
            .length_percentage()
            .expect("computed transform-origin lost its style value")
    };
    let origin_x = origin_lp(&transform_values.transform_origin_x).to_px(reference_box.width);
    let origin_y = origin_lp(&transform_values.transform_origin_y).to_px(reference_box.height);
    let origin_z = origin_lp(&transform_values.transform_origin_z)
        .to_px(CssPixels::from_raw(0))
        .to_float();

    // 1. Start with the identity matrix.
    // 2. Translate by the computed X, Y, and Z values of transform-origin.
    // 3. Translate by the computed X, Y, and Z values of translate.
    // 4. Rotate by the computed <angle> about the specified axis of rotate.
    // 5. Scale by the computed X, Y, and Z values of scale.
    // 6. Translate and rotate by the transform specified by offset.
    // 7. Multiply by each of the transform functions in transform from left to right.
    // NB: The resolved transform list carries translate, rotate, scale, and the
    //     transform functions pre-lowered in exactly that order, so the offset
    //     transform goes after the entries of the individual transform properties.
    let individual_transform_count = [
        &transform_values.translate,
        &transform_values.rotate,
        &transform_values.scale,
    ]
    .into_iter()
    .filter(|handle| !handle.pointer.is_null())
    .count();
    let (individual_transforms, transform_functions) = transform_values
        .resolved_transforms
        .as_slice()
        .split_at(individual_transform_count);
    let mut matrix = multiply_transform_functions(
        translation_matrix(0.0, 0.0, origin_z),
        individual_transforms,
        reference_box,
    );
    if let Some(offset_transform) =
        compute_offset_transform(layout_arena, node, style, reference_box, origin_x, origin_y)
    {
        matrix = matrix.multiplied(offset_transform);
    }
    matrix = multiply_transform_functions(matrix, transform_functions, reference_box);

    // The x and y properties of <use> define an additional translation applied after any
    // transformations specified with other properties.
    if let Some(additional) = additional_element_transform {
        matrix = matrix.multiplied(affine_to_matrix(additional));
    }

    // 8. Translate by the negated computed X, Y and Z values of transform-origin.
    matrix = matrix.multiplied(translation_matrix(0.0, 0.0, -origin_z));

    let scale = pixel_ratio as f32;
    let device_origin = FloatPoint {
        x: (reference_box.x + origin_x).to_float() * scale,
        y: (reference_box.y + origin_y).to_float() * scale,
    };
    let matrix = scale_matrix_for_device_pixels(matrix, scale);
    let is_invertible = matrix.is_invertible();
    Some((
        TransformData {
            matrix,
            origin: device_origin,
            sorting_context_root_index: None,
            flattens_inherited_transform: false,
            role: TransformDataRole::CssTransform,
            synthetic_plane: false,
            establishes_sorting_context: false,
        },
        is_invertible,
    ))
}

// https://drafts.csswg.org/css-transforms-2/#perspective-matrix
pub(crate) fn compute_perspective_data(
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
    pixel_ratio: f64,
) -> Option<PerspectiveData> {
    let node = slot;
    let style = layout_arena.node_style_if_live(node)?;
    let transform_values = style.transform();
    if !transform_values.has_perspective || !style_queries::is_transformable(layout_arena, node) {
        return None;
    }

    // The perspective matrix is computed as follows:

    // 1. Start with the identity matrix.
    // 2. Translate by the computed X and Y values of 'perspective-origin'
    // https://drafts.csswg.org/css-transforms-2/#perspective-origin-property
    // Percentages: refer to the size of the reference box
    let reference_box = transform_reference_box(style, layout_arena, slot);
    let origin_x = transform_values
        .perspective_origin_x
        .length_percentage()
        .expect("computed perspective-origin lost its style value")
        .to_px(reference_box.width);
    let origin_y = transform_values
        .perspective_origin_y
        .length_percentage()
        .expect("computed perspective-origin lost its style value")
        .to_px(reference_box.height);
    let computed_x = (reference_box.x + origin_x).to_float();
    let computed_y = (reference_box.y + origin_y).to_float();
    let mut matrix = translation_matrix(computed_x, computed_y, 0.0);

    // 3. Multiply by the matrix that would be obtained from the 'perspective()' transform function, where the
    //    length is provided by the value of the perspective property
    // https://drafts.csswg.org/css-transforms-2/#funcdef-perspective
    // If the depth value is less than '1px', it must be treated as '1px' for the purpose of rendering, [..]
    let distance = CssPixels::from_raw(transform_values.perspective_px).to_float().max(1.0);
    matrix = matrix.multiplied(perspective_matrix(distance));

    // 4. Translate by the negated computed X and Y values of 'perspective-origin'
    matrix = matrix.multiplied(translation_matrix(-computed_x, -computed_y, 0.0));
    Some(PerspectiveData {
        matrix: scale_matrix_for_device_pixels(matrix, pixel_ratio as f32),
        flattens_inherited_transform: false,
    })
}

pub(crate) fn compute_css_clip_data(
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
    pixel_ratio: f64,
) -> Option<ClipData> {
    let node = slot;
    let style = layout_arena.node_style_if_live(node)?;
    if !style.effects().clip_is_rect || !style.is_absolutely_positioned() {
        return None;
    }
    let border_box = paintable_geometry::absolute_border_box_rect(layout_arena, slot);
    let [top_edge, right_edge, bottom_edge, left_edge] = &style.effects().clip_edges;
    let left = border_box.x + resolved_clip_edge_px(left_edge, CssPixels::from_raw(0));
    let top = border_box.y + resolved_clip_edge_px(top_edge, CssPixels::from_raw(0));
    let right = border_box.x + resolved_clip_edge_px(right_edge, border_box.width);
    let bottom = border_box.y + resolved_clip_edge_px(bottom_edge, border_box.height);
    let resolved = CssPixelRect::new(left, top, right - left, bottom - top);
    let effective = if resolved.width < CssPixels::from_raw(0) || resolved.height < CssPixels::from_raw(0) {
        CssPixelRect::new(
            CssPixels::from_raw(0),
            CssPixels::from_raw(0),
            CssPixels::from_raw(0),
            CssPixels::from_raw(0),
        )
    } else {
        resolved
    };
    let converter = DevicePixelConverter::new(pixel_ratio);
    Some(ClipData {
        rect: converter.rounded_device_rect(effective).to_float(),
        corner_radii: CornerRadii::default(),
        mode: super::ClipMode::Intersect,
    })
}

fn resolved_clip_edge_px(edge: &ComputedClipEdge, auto_value: CssPixels) -> CssPixels {
    if edge.is_auto {
        return auto_value;
    }
    let ratio = crate::css::style_compute::LENGTH_UNIT_CANONICAL_PX_RATIOS[edge.unit as usize];
    assert!(ratio.is_finite(), "computed clip edge is not an absolute length");
    CssPixels::nearest_value_for(edge.value * ratio)
}

fn border_radius_pair(handle: &ComputedStyleValueHandle) -> (LengthPercentageRef<'_>, LengthPercentageRef<'_>) {
    let Some(value) = style_queries::handle_value(handle) else {
        unreachable!("computed border-radius lost its style value");
    };
    border_radius_pair_of_value(value)
}

pub(crate) fn border_radius_pair_of_value(
    value: &StyleValueData,
) -> (LengthPercentageRef<'_>, LengthPercentageRef<'_>) {
    let StyleValueData::BorderRadius {
        horizontal_radius,
        vertical_radius,
        ..
    } = value
    else {
        unreachable!("computed border-radius is a border-radius value");
    };
    (
        LengthPercentageRef::over(horizontal_radius.data()),
        LengthPercentageRef::over(vertical_radius.data()),
    )
}

fn border_radius_is_initial(handle: &ComputedStyleValueHandle) -> bool {
    let Some(StyleValueData::BorderRadius {
        horizontal_radius,
        vertical_radius,
        ..
    }) = style_queries::handle_value(handle)
    else {
        unreachable!("computed border-radius lost its style value");
    };
    let is_zero_px = |value: &StyleValueData| {
        matches!(value, StyleValueData::Length { value, unit }
            if *value == 0.0 && *unit == crate::css::style_compute::px_length_unit())
    };
    is_zero_px(horizontal_radius.data()) && is_zero_px(vertical_radius.data())
}

pub(crate) fn border_radii_data(
    style: ComputedValuesView<'_>,
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
) -> BorderRadii {
    let border = style.border();
    let has_noninitial_radii = !border_radius_is_initial(&border.border_bottom_left_radius)
        || !border_radius_is_initial(&border.border_bottom_right_radius)
        || !border_radius_is_initial(&border.border_top_left_radius)
        || !border_radius_is_initial(&border.border_top_right_radius);
    if !has_noninitial_radii {
        return BorderRadii::default();
    }
    let border_box = paintable_geometry::absolute_border_box_rect(layout_arena, slot);
    let border_rect = CssPixelRect::new(
        CssPixels::from_raw(0),
        CssPixels::from_raw(0),
        border_box.width,
        border_box.height,
    );
    crate::painting::border_radii::normalize_border_radii_data(
        border_rect,
        border_rect,
        [
            border_radius_pair(&border.border_top_left_radius),
            border_radius_pair(&border.border_top_right_radius),
            border_radius_pair(&border.border_bottom_right_radius),
            border_radius_pair(&border.border_bottom_left_radius),
        ],
    )
}

pub(crate) fn padding_edge_border_radii(
    style: ComputedValuesView<'_>,
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
) -> BorderRadii {
    border_radii_data(style, layout_arena, slot).shrunken(
        style.border_top_width(),
        style.border_right_width(),
        style.border_bottom_width(),
        style.border_left_width(),
    )
}

pub(crate) fn piece_border_radii_data(
    style: ComputedValuesView<'_>,
    piece_width: CssPixels,
    piece_height: CssPixels,
    present_edges: u8,
) -> BorderRadii {
    let border = style.border();
    let has_noninitial_radii = !border_radius_is_initial(&border.border_bottom_left_radius)
        || !border_radius_is_initial(&border.border_bottom_right_radius)
        || !border_radius_is_initial(&border.border_top_left_radius)
        || !border_radius_is_initial(&border.border_top_right_radius);
    if !has_noninitial_radii {
        return BorderRadii::default();
    }
    let border_rect = CssPixelRect::new(
        CssPixels::from_raw(0),
        CssPixels::from_raw(0),
        piece_width,
        piece_height,
    );
    let mut radii = crate::painting::border_radii::resolve_corner_radii(
        border_rect,
        [
            border_radius_pair(&border.border_top_left_radius),
            border_radius_pair(&border.border_top_right_radius),
            border_radius_pair(&border.border_bottom_right_radius),
            border_radius_pair(&border.border_bottom_left_radius),
        ],
    );
    // A corner only keeps its radius when the fragment piece retains both of the box's edges
    // meeting there; corners cut by fragmentation are square.
    const TOP_EDGE: u8 = 1 << 0;
    const RIGHT_EDGE: u8 = 1 << 1;
    const BOTTOM_EDGE: u8 = 1 << 2;
    const LEFT_EDGE: u8 = 1 << 3;
    let zero = CssPixels::from_raw(0);
    let corner_edges = [
        TOP_EDGE | LEFT_EDGE,
        TOP_EDGE | RIGHT_EDGE,
        BOTTOM_EDGE | RIGHT_EDGE,
        BOTTOM_EDGE | LEFT_EDGE,
    ];
    for (corner, edges) in corner_edges.into_iter().enumerate() {
        if present_edges & edges != edges {
            radii.values[corner * 2] = zero;
            radii.values[corner * 2 + 1] = zero;
        }
    }
    crate::painting::border_radii::scale_radii_to_fit(border_rect, radii)
}

fn overflow_property_applies(layout_arena: &impl PaintRead, slot: NodeSlotId) -> bool {
    // https://drafts.csswg.org/css-overflow-3/#overflow-control
    // Overflow properties apply to block containers, flex containers and grid containers.
    // FIXME: Ideally we would check whether overflow applies positively rather than listing exceptions. However,
    // not all elements that should support overflow are currently identifiable that way.
    if layout_arena.node_kind_if_live(slot).is_some_and(node_painting::is_svg) {
        return false;
    }
    let display = crate::painting::style_queries::display(layout_arena, slot);
    if node_painting::is_fragmented_inline(layout_arena, slot) {
        return false;
    }
    if display.is_ruby_inside() {
        return false;
    }
    if display.is_internal() && !display.is_table_cell() && !display.is_table_caption() {
        return false;
    }
    true
}

// https://drafts.csswg.org/css-overflow-4/#overflow-clip-edge
fn overflow_clip_edge_rect(
    style: ComputedValuesView<'_>,
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
) -> CssPixelRect {
    use crate::css::css_enums::background_box::{BORDER_BOX, CONTENT_BOX};
    let overflow_clip_margin = &style.misc_reset().overflow_clip_margin;
    // https://drafts.csswg.org/css-overflow-4/#overflow-clip-margin
    // Values are defined as follows:
    // '<visual-box>'
    // Specifies the box edge to use as the overflow clip edge origin, i.e. when the specified offset is zero.
    // If omitted, defaults to 'padding-box' on non-replaced elements, or 'content-box' on replaced elements.
    let top_side = &overflow_clip_margin.top;
    let visual_box = if top_side.has_visual_box {
        top_side.visual_box
    } else {
        let node_kind = layout_arena.node_kind_if_live(slot);
        if node_kind.is_some_and(node_facts::kind_is_replaced_box) {
            CONTENT_BOX
        } else {
            crate::css::css_enums::background_box::PADDING_BOX
        }
    };
    let overflow_clip_edge = match visual_box {
        CONTENT_BOX => paintable_geometry::absolute_rect(layout_arena, slot),
        BORDER_BOX => paintable_geometry::absolute_border_box_rect(layout_arena, slot),
        _ => paintable_geometry::absolute_padding_box_rect(layout_arena, slot),
    };
    // '<length [0,∞]>'
    // The specified offset dictates how much the overflow clip edge is expanded from the specified box edge
    // Negative values are invalid. Defaults to zero if omitted.
    overflow_clip_edge.inflated(
        overflow_clip_margin.top.offset,
        overflow_clip_margin.right.offset,
        overflow_clip_margin.bottom.offset,
        overflow_clip_margin.left.offset,
    )
}

pub(crate) fn mix_blend_mode_to_compositing_and_blending_operator(
    mix_blend_mode: u8,
) -> CompositingAndBlendingOperator {
    use crate::css::css_enums::mix_blend_mode as css;
    match mix_blend_mode {
        css::NORMAL => CompositingAndBlendingOperator::Normal,
        css::MULTIPLY => CompositingAndBlendingOperator::Multiply,
        css::SCREEN => CompositingAndBlendingOperator::Screen,
        css::OVERLAY => CompositingAndBlendingOperator::Overlay,
        css::DARKEN => CompositingAndBlendingOperator::Darken,
        css::LIGHTEN => CompositingAndBlendingOperator::Lighten,
        css::COLOR_DODGE => CompositingAndBlendingOperator::ColorDodge,
        css::COLOR_BURN => CompositingAndBlendingOperator::ColorBurn,
        css::HARD_LIGHT => CompositingAndBlendingOperator::HardLight,
        css::SOFT_LIGHT => CompositingAndBlendingOperator::SoftLight,
        css::DIFFERENCE => CompositingAndBlendingOperator::Difference,
        css::EXCLUSION => CompositingAndBlendingOperator::Exclusion,
        css::HUE => CompositingAndBlendingOperator::Hue,
        css::SATURATION => CompositingAndBlendingOperator::Saturation,
        css::COLOR => CompositingAndBlendingOperator::Color,
        css::LUMINOSITY => CompositingAndBlendingOperator::Luminosity,
        css::PLUS_DARKER => CompositingAndBlendingOperator::PlusDarker,
        css::PLUS_LIGHTER => CompositingAndBlendingOperator::PlusLighter,
        _ => unreachable!("computed mix-blend-mode holds an unknown keyword"),
    }
}

/// The referenced filter's region, in the filtered element's user space: the element's border
/// box, or the whole enclosing viewport rect for an element without geometry of its own.
// The bounds size the transparent fill that triggers a content-generating SVG filter, which
// the stacking-context preamble records.
fn set_svg_filter_bounds(
    layout_arena: &impl PaintableRowsRead,
    slot: NodeSlotId,
    bounds: Option<crate::layout::used_values::FfiCssPixelRect>,
) {
    let previous = std::mem::replace(
        &mut layout_arena.committed_side_data_mut(slot).svg_filter_bounds,
        bounds,
    );
    if previous != bounds {
        use crate::painting::record::damage::PaintDamage;
        layout_arena.push_paint_damage(slot, PaintDamage::SCOPE_PREAMBLE | PaintDamage::SVG);
    }
}

fn svg_filter_bounds(layout_arena: &impl PaintRead, slot: NodeSlotId) -> Option<CssPixelRect> {
    let bounds = paintable_geometry::absolute_border_box_rect(layout_arena, slot);
    if !bounds.is_empty() {
        return Some(bounds);
    }
    crate::painting::svg_viewport::nearest_svg_viewport_user_rect(layout_arena, slot).map(|rect| {
        CssPixelRect::new(
            CssPixels::nearest_value_for(f64::from(rect.x)),
            CssPixels::nearest_value_for(f64::from(rect.y)),
            CssPixels::nearest_value_for(f64::from(rect.width)),
            CssPixels::nearest_value_for(f64::from(rect.height)),
        )
    })
}

fn published_svg_filter(
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
    kind: crate::painting::svg_paint_resources::SvgPaintResourceKind,
    style: ComputedValuesView<'_>,
    device_pixels_per_css_pixel: f64,
) -> crate::painting::host::visual_context::ResolvedSvgFilter {
    let Some(published) = layout_arena.published_svg_filter(slot, kind) else {
        return crate::painting::host::visual_context::ResolvedSvgFilter {
            failed: true,
            ..Default::default()
        };
    };
    if published.failed {
        return crate::painting::host::visual_context::ResolvedSvgFilter {
            failed: true,
            ..Default::default()
        };
    }
    let mut builder = crate::painting::svg_filter::SvgFilterGraphBuilder::new(device_pixels_per_css_pixel);
    if layout_arena.paintable_row_is_populated(slot) {
        let absolute_rect = paintable_geometry::absolute_rect(layout_arena, slot);
        let dest_rect = libgfx_rust::enclosing_int_rect(libgfx_rust::FloatRect::new(
            absolute_rect.x.to_float(),
            absolute_rect.y.to_float(),
            absolute_rect.width.to_float(),
            absolute_rect.height.to_float(),
        ));
        builder.set_image_target(dest_rect, style.image_rendering());
    }
    for primitive in published.primitives.iter().cloned() {
        builder.push(primitive);
    }
    crate::painting::host::visual_context::ResolvedSvgFilter {
        failed: false,
        filter: builder.finish(),
        svg_filter_bounds: svg_filter_bounds(layout_arena, slot)
            .map(crate::layout::used_values::FfiCssPixelRect::from)
            .into(),
    }
}

pub(crate) fn compute_effects_data(
    layout_arena: &impl PaintableRowsRead,
    slot: NodeSlotId,
    device_pixels_per_css_pixel: f64,
) -> Option<EffectsData> {
    let resolved = resolve_effects_data(layout_arena, slot, device_pixels_per_css_pixel)?;
    set_svg_filter_bounds(layout_arena, slot, resolved.svg_filter_bounds);
    resolved.effects
}

/// A box's effects, and the bounds of the SVG filter it references.
pub(crate) struct ResolvedEffects {
    pub(crate) effects: Option<EffectsData>,
    pub(crate) svg_filter_bounds: Option<crate::layout::used_values::FfiCssPixelRect>,
}

/// Resolves a box's effects, or `None` for a box without style.
pub(crate) fn resolve_effects_data(
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
    device_pixels_per_css_pixel: f64,
) -> Option<ResolvedEffects> {
    use crate::css::css_enums::mix_blend_mode;
    let style = layout_arena.node_style_if_live(slot)?;
    let effects_values = style.effects();
    let mut svg_filter_bounds = None;
    let filter = if crate::painting::css_filter::contains_url(&effects_values.filter) {
        let resolved_svg_filter = published_svg_filter(
            layout_arena,
            slot,
            crate::painting::svg_paint_resources::SvgPaintResourceKind::Filter,
            style,
            device_pixels_per_css_pixel,
        );
        svg_filter_bounds = resolved_svg_filter
            .svg_filter_bounds
            .has_value
            .then_some(resolved_svg_filter.svg_filter_bounds.value);
        crate::painting::css_filter::serialize_filter_with_resolved_svg(
            &effects_values.filter,
            resolved_svg_filter,
            device_pixels_per_css_pixel,
        )
        .map(std::sync::Arc::new)
    } else {
        crate::painting::css_filter::serialize_non_url_filter(&effects_values.filter, device_pixels_per_css_pixel)
            .map(std::sync::Arc::new)
    };
    let backdrop_filter = compute_backdrop_filter_data(layout_arena, slot, style, device_pixels_per_css_pixel);
    let keeps_effects_node_for_later_values = layout_arena
        .node_has_compositor_animation_frame(slot, crate::layout::node_data::CompositorAnimationFrameKind::Opacity)
        || style_queries::will_change_promotes_effects_node(style);
    if filter.is_none()
        && backdrop_filter.is_none()
        && effects_values.opacity == 1.0
        && effects_values.mix_blend_mode == mix_blend_mode::NORMAL
        && !keeps_effects_node_for_later_values
    {
        return Some(ResolvedEffects {
            effects: None,
            svg_filter_bounds,
        });
    }
    let effects = EffectsData {
        opacity: effects_values.opacity,
        blend_mode: mix_blend_mode_to_compositing_and_blending_operator(effects_values.mix_blend_mode),
        filter,
        backdrop_filter,
    };
    let needs_effects_node = effects.needs_layer() || keeps_effects_node_for_later_values;
    Some(ResolvedEffects {
        effects: needs_effects_node.then_some(effects),
        svg_filter_bounds,
    })
}

// https://drafts.fxtf.org/filter-effects-2/#BackdropFilterProperty
fn compute_backdrop_filter_data(
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
    style: ComputedValuesView<'_>,
    device_pixels_per_css_pixel: f64,
) -> Option<super::BackdropFilterData> {
    let backdrop_filter = &style.effects().backdrop_filter;
    if backdrop_filter.operations.length == 0 {
        return None;
    }
    let converter = DevicePixelConverter::new(device_pixels_per_css_pixel);
    let region = converter.rounded_device_rect(paintable_geometry::absolute_border_box_rect(layout_arena, slot));
    if region.is_empty() {
        return None;
    }
    let filter = if crate::painting::css_filter::contains_url(backdrop_filter) {
        let resolved_svg_filter = published_svg_filter(
            layout_arena,
            slot,
            crate::painting::svg_paint_resources::SvgPaintResourceKind::BackdropFilter,
            style,
            device_pixels_per_css_pixel,
        );
        crate::painting::css_filter::serialize_filter_with_resolved_svg(
            backdrop_filter,
            resolved_svg_filter,
            device_pixels_per_css_pixel,
        )
    } else {
        crate::painting::css_filter::serialize_non_url_filter(backdrop_filter, device_pixels_per_css_pixel)
    }?;
    Some(super::BackdropFilterData {
        filter: std::sync::Arc::new(filter),
        region,
        corner_radii: border_radii_data(style, layout_arena, slot).as_corners(&converter),
    })
}

pub(crate) struct MaskLayerPresenceEntry {
    pub origin: super::MaskLayerOrigin,
    pub area: CssPixelRect,
    pub kind: libgfx_rust::MaskKind,
}

pub(crate) fn mask_layer_presence(
    layout_arena: &impl PaintRead,
    slot: NodeSlotId,
    include_css_mask_layers: bool,
) -> Vec<MaskLayerPresenceEntry> {
    use super::MaskLayerOrigin;
    let mut layers = Vec::new();
    if include_css_mask_layers {
        let node = slot;
        if let Some(style) = layout_arena.node_style_if_live(node)
            && style_queries::mask_layers_have_image(style.mask())
            && layout_arena
                .node_kind_if_live(node)
                .is_some_and(node_facts::kind_is_box)
            && style_queries::establishes_stacking_context(layout_arena, node)
        {
            layers.push(MaskLayerPresenceEntry {
                origin: MaskLayerOrigin::CssMaskLayers,
                area: paintable_geometry::absolute_border_box_rect(layout_arena, slot),
                kind: libgfx_rust::MaskKind::Alpha,
            });
        }
    }
    if layout_arena
        .node_kind_if_live(slot)
        .is_some_and(node_painting::supports_svg_masking)
    {
        if let Some(mask_area) = crate::painting::svg_masking::mask_area(layout_arena, slot) {
            layers.push(MaskLayerPresenceEntry {
                origin: MaskLayerOrigin::SvgMask,
                area: mask_area,
                kind: crate::painting::svg_masking::mask_kind(layout_arena, slot),
            });
        }
        if let Some(clip_area) = crate::painting::svg_masking::clip_area(layout_arena, slot) {
            layers.push(MaskLayerPresenceEntry {
                origin: MaskLayerOrigin::SvgClip,
                area: clip_area,
                kind: libgfx_rust::MaskKind::Alpha,
            });
        }
    }
    layers
}

pub(crate) fn backface_hidden(layout_arena: &impl PaintRead, node: NodeSlotId) -> bool {
    use crate::css::css_enums::backface_visibility;
    let Some(style) = layout_arena.node_style_if_live(node) else {
        return false;
    };
    style.transform().backface_visibility == backface_visibility::HIDDEN
        && style_queries::is_transformable(layout_arena, node)
}

pub(crate) fn may_have_clip(layout_arena: &impl PaintRead, node: NodeSlotId) -> bool {
    use crate::css::css_enums::{content_visibility, overflow};
    let Some(style) = layout_arena.node_style_if_live(node) else {
        return false;
    };
    let box_values = style.box_values();
    box_values.overflow_x != overflow::VISIBLE
        || box_values.overflow_y != overflow::VISIBLE
        || box_values.paint_containment
        || style.content_visibility() == content_visibility::AUTO
}

pub(crate) fn compute_clip_data(layout_arena: &impl PaintRead, slot: NodeSlotId, pixel_ratio: f64) -> Option<ClipData> {
    use crate::css::css_enums::{content_visibility, overflow};
    let node = slot;
    let style = layout_arena.node_style_if_live(node)?;
    let box_values = style.box_values();
    let mut overflow_x = box_values.overflow_x;
    let mut overflow_y = box_values.overflow_y;

    let style_has_paint_containment =
        box_values.paint_containment || style.content_visibility() == content_visibility::AUTO;
    // https://drafts.csswg.org/css-contain-2/#paint-containment
    // 1. The contents of the element including any ink or scrollable overflow must be clipped to the overflow clip
    // edge of the paint containment box, taking corner clipping into account. This does not include the creation of
    // any mechanism to access or indicate the presence of the clipped content; nor does it inhibit the creation of
    // any such mechanism through other properties, such as overflow, resize, or text-overflow.
    // NOTE: This clipping shape respects overflow-clip-margin, allowing an element with paint containment
    // to still slightly overflow its normal bounds.
    if style_has_paint_containment && style_queries::has_paint_containment(layout_arena, node, style) {
        // NOTE: The behavior described in this paragraph is equivalent to changing 'overflow-x: visible' into
        // 'overflow-x: clip' and 'overflow-y: visible' into 'overflow-y: clip' at used value time, while leaving
        // other values of 'overflow-x' and 'overflow-y' unchanged.
        overflow_x = overflow::CLIP;
        overflow_y = overflow::CLIP;
    }

    // https://drafts.csswg.org/css-overflow-3/#propdef-overflow
    // 'clip'
    // This value indicates that the box’s content is clipped to its overflow clip edge
    let has_hidden_overflow = overflow_x != overflow::VISIBLE || overflow_y != overflow::VISIBLE;
    if !has_hidden_overflow || !overflow_property_applies(layout_arena, slot) {
        return None;
    }

    let padding_box = paintable_geometry::absolute_padding_box_rect(layout_arena, slot);
    let overflow_clip_edge = overflow_clip_edge_rect(style, layout_arena, slot);
    let extent_limit = CssPixels::from_integer(crate::css::css_pixels::MAX_INTEGER_VALUE as i64);
    let (left, right) = match overflow_x {
        overflow::VISIBLE => (CssPixels::from_raw(0), extent_limit),
        overflow::CLIP => (overflow_clip_edge.x, overflow_clip_edge.x + overflow_clip_edge.width),
        _ => (padding_box.x, padding_box.x + padding_box.width),
    };
    let (top, bottom) = match overflow_y {
        overflow::VISIBLE => (CssPixels::from_raw(0), extent_limit),
        overflow::CLIP => (overflow_clip_edge.y, overflow_clip_edge.y + overflow_clip_edge.height),
        _ => (padding_box.y, padding_box.y + padding_box.height),
    };
    let clip_rect = CssPixelRect::new(left, top, right - left, bottom - top);

    // https://drafts.csswg.org/css-overflow-3/#corner-clipping
    // As mentioned in CSS Backgrounds 3 § 4.3 Corner Clipping, the clipping region established by 'overflow' can be
    // rounded:
    // - When 'overflow-x' and 'overflow-y' compute to 'hidden', 'scroll', or 'auto', the clipping region is rounded
    //   based on the border radius, adjusted to the padding edge, as described in CSS Backgrounds 3 § 4.2 Corner
    //   Shaping.
    // - When both 'overflow-x' and 'overflow-y' compute to 'clip', the clipping region is rounded as described in § 3.2
    //   Expanding Clipping Bounds: the 'overflow-clip-margin' property.
    // - However, when one of 'overflow-x' or 'overflow-y' computes to 'clip' and the other computes to 'visible', the
    //   clipping region is not rounded.
    // FIXME: Adjust the border radii for the overflow-clip-margin case.
    //        (see https://drafts.csswg.org/css-overflow-4/#valdef-overflow-clip-margin-length-0 )
    let radii = if overflow_x != overflow::VISIBLE && overflow_y != overflow::VISIBLE {
        padding_edge_border_radii(style, layout_arena, slot)
    } else {
        BorderRadii::default()
    };
    let converter = DevicePixelConverter::new(pixel_ratio);
    Some(ClipData {
        rect: converter.rounded_device_rect(clip_rect).to_float(),
        corner_radii: radii.as_corners(&converter),
        mode: super::ClipMode::Intersect,
    })
}
