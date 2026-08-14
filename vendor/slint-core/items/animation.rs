// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

/*!
This module contains the `AnimationImage` item — a Lumen fork addition
(see docs/performance-fork-design.md).

The item displays a frame of a pre-rendered sequence whose pixels live
OUTSIDE the reactive scene graph. The application registers the frames once
through [`crate::api::Window::register_animation_frames`] and switches the
visible frame with [`crate::api::Window::set_animation_frame`]. Neither call
invalidates a property binding or re-evaluates the tree: the renderer only
repaints the item's own rectangle (via `mark_dirty_region`) and re-uploads the
frame content into its texture cache entry. The rest of the scene (background,
HUD, seats) is never re-rendered.
*/

use super::{ImageRendering, Item, ItemConsts, ItemRc, RenderingResult};
use crate::input::{
    FocusEvent, FocusEventResult, InputEventFilterResult, InputEventResult, InternalKeyEvent,
    KeyEventResult, MouseEvent,
};
use crate::item_rendering::ItemRenderer;
use crate::item_rendering::CachedRenderingData;
use crate::layout::{LayoutInfo, Orientation};
use crate::lengths::{LogicalLength, LogicalRect, LogicalSize};
use crate::window::WindowAdapter;
use crate::{Coord, Property};
use alloc::rc::Rc;
use const_field_offset::FieldOffsets;
use core::pin::Pin;
use i_slint_core_macros::*;

#[repr(C)]
#[derive(FieldOffsets, Default, SlintElement)]
#[pin]
/// The implementation of the `AnimationImage` element
pub struct AnimationImage {
    pub width: Property<LogicalLength>,
    pub height: Property<LogicalLength>,
    /// The key under which the application registered the pre-rendered frame
    /// sequence (`slint::Window::register_animation_frames`). The frame shown
    /// is switched with `slint::Window::set_animation_frame`, without
    /// invalidating the scene tree.
    pub animation_key: Property<i32>,
    pub image_rendering: Property<ImageRendering>,
    pub cached_rendering_data: CachedRenderingData,
}

impl Item for AnimationImage {
    fn init(self: Pin<&Self>, _self_rc: &ItemRc) {}

    fn deinit(self: Pin<&Self>, _window_adapter: &Rc<dyn WindowAdapter>) {}

    fn layout_info(
        self: Pin<&Self>,
        _orientation: Orientation,
        _cross_axis_constraint: Coord,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
    ) -> LayoutInfo {
        // The frame size is not known to the scene graph (it lives in the
        // renderer's registry); size is driven by explicit width/height.
        LayoutInfo { stretch: 1., ..LayoutInfo::default() }
    }

    fn input_event_filter_before_children(
        self: Pin<&Self>,
        _: &MouseEvent,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
        _: &mut super::MouseCursor,
    ) -> InputEventFilterResult {
        InputEventFilterResult::ForwardAndIgnore
    }

    fn input_event(
        self: Pin<&Self>,
        _: &MouseEvent,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
        _: &mut super::MouseCursor,
    ) -> InputEventResult {
        InputEventResult::EventIgnored
    }

    fn capture_key_event(
        self: Pin<&Self>,
        _: &InternalKeyEvent,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
    ) -> KeyEventResult {
        KeyEventResult::EventIgnored
    }

    fn key_event(
        self: Pin<&Self>,
        _: &InternalKeyEvent,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
    ) -> KeyEventResult {
        KeyEventResult::EventIgnored
    }

    fn focus_event(
        self: Pin<&Self>,
        _: &FocusEvent,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
    ) -> FocusEventResult {
        FocusEventResult::FocusIgnored
    }

    fn render(
        self: Pin<&Self>,
        backend: &mut &mut dyn ItemRenderer,
        self_rc: &ItemRc,
        size: LogicalSize,
    ) -> RenderingResult {
        (*backend).draw_animation_image(self, self_rc, size);
        RenderingResult::ContinueRenderingChildren
    }

    fn bounding_rect(
        self: core::pin::Pin<&Self>,
        _window_adapter: &Rc<dyn WindowAdapter>,
        _self_rc: &ItemRc,
        geometry: LogicalRect,
    ) -> LogicalRect {
        geometry
    }

    fn clips_children(self: core::pin::Pin<&Self>) -> bool {
        false
    }
}

impl ItemConsts for AnimationImage {
    const cached_rendering_data_offset: const_field_offset::FieldOffset<
        AnimationImage,
        CachedRenderingData,
    > = AnimationImage::FIELD_OFFSETS.cached_rendering_data().as_unpinned_projection();
}
