use std::cell::RefCell;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::iter::zip;
use std::rc::Rc;

use niri_config::{Action, Bind, BoundAction, Config, Key, Modifiers, Trigger};
use niri_ipc::SizeChange;
use smithay::backend::input::TouchSlot;
use smithay::backend::renderer::element::Kind;
use smithay::desktop::{Space, Window};
use smithay::input::keyboard::Keysym;
use smithay::output::{Output, WeakOutput};
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::layout::floating::DIRECTIONAL_MOVE_PX;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::utils::to_physical_precise_round;

pub const SELECTION_BORDER: i32 = 2;

// Ideally the region select UI should support cross-output selections. However, that poses some
// technical challenges when the outputs have different scales and such. So, this implementation
// allows only single-output selections for now.
//
// As a consequence of this, selection coordinates are in output-local coordinate space.
#[allow(clippy::large_enum_variant)]
pub enum RegionSelectUi {
    Closed {
        last_selection: Option<(WeakOutput, Rectangle<i32, Physical>)>,
        clock: Clock,
        config: Rc<RefCell<Config>>,
    },
    Open {
        selection: (Output, Point<i32, Physical>, Point<i32, Physical>),
        output_data: HashMap<Output, OutputData>,
        button: Button,
        open_anim: Animation,
        clock: Clock,
        config: Rc<RefCell<Config>>,
    },
}

/// State for moving the selection (as opposed to just drawing).
pub struct MoveState {
    // Cursor offset from selection.1 when starting the move.
    pub pointer_offset: Point<i32, Physical>,
    // If the move is initiated by a touch, this is the slot. If `None`, the move was initiated by
    // holding Space.
    pub touch_slot: Option<TouchSlot>,
}

pub enum Button {
    Up,
    Down {
        touch_slot: Option<TouchSlot>,
        last_pos: (Output, Point<i32, Physical>),
        move_state: Option<MoveState>,
    },
}

pub struct OutputData {
    pub size: Size<i32, Physical>,
    pub scale: f64,
    pub transform: Transform,
    pub buffers: [SolidColorBuffer; 8],
    pub locations: [Point<i32, Physical>; 8],
}

impl Button {
    pub fn is_down(&self) -> bool {
        matches!(self, Self::Down { .. })
    }

    pub fn is_dragging_selection(&self) -> bool {
        matches!(self, Self::Down { .. })
    }
}

impl RegionSelectUi {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self::Closed {
            last_selection: None,
            clock,
            config,
        }
    }

    pub fn open(
        &mut self,
        outputs: impl IntoIterator<Item = Output>,
        default_output: Output,
    ) -> bool {
        let Self::Closed {
            last_selection,
            clock,
            config,
        } = self
        else {
            return false;
        };

        let output_list: Vec<Output> = outputs.into_iter().collect();
        if output_list.is_empty() {
            return false;
        }

        let last_selection = last_selection
            .take()
            .and_then(|(weak, sel)| weak.upgrade().map(|output| (output, sel)));
        let selection = match last_selection {
            Some(selection) if output_list.contains(&selection.0) => selection,
            _ => {
                let output = default_output;
                let output_transform = output.current_transform();
                let output_mode = output.current_mode().unwrap();
                let size = output_transform.transform_size(output_mode.size);
                (
                    output,
                    Rectangle::new(
                        Point::from((size.w / 4, size.h / 4)),
                        Size::from((size.w / 2, size.h / 2)),
                    ),
                )
            }
        };

        let selection = (
            selection.0,
            selection.1.loc,
            selection.1.loc + selection.1.size - Size::from((1, 1)),
        );

        let output_data = output_list
            .into_iter()
            .map(|output| {
                let transform = output.current_transform();
                let output_mode = output.current_mode().unwrap();
                let size = transform.transform_size(output_mode.size);
                let scale = output.current_scale().fractional_scale();
                let buffers = [
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                ];
                let locations = [Default::default(); 8];

                let data = OutputData {
                    size,
                    scale,
                    transform,
                    buffers,
                    locations,
                };
                (output, data)
            })
            .collect();

        let open_anim = {
            let c = config.borrow();
            Animation::new(clock.clone(), 0., 1., 0., c.animations.screenshot_ui_open.0)
        };

        *self = Self::Open {
            selection,
            output_data,
            button: Button::Up,
            open_anim,
            clock: clock.clone(),
            config: config.clone(),
        };

        self.update_buffers();

        true
    }

    pub fn close(&mut self) -> bool {
        let Self::Open {
            selection,
            clock,
            config,
            ..
        } = self
        else {
            return false;
        };

        let last_selection = Some((
            selection.0.downgrade(),
            rect_from_corner_points(selection.1, selection.2),
        ));

        *self = Self::Closed {
            last_selection,
            clock: clock.clone(),
            config: config.clone(),
        };

        true
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    pub fn binds(&self) -> impl Iterator<Item = &Bind> + Clone {
        const fn bind(key: Key, action: Action) -> Bind {
            Bind {
                key,
                action: BoundAction::Press(action),
                repeat: false, // Not useful for these actions.
                cooldown: None,
                allow_when_locked: true,
                allow_inhibiting: false,
                hotkey_overlay_title: None,
            }
        }

        const BINDS: &[Bind] = &[
            bind(
                Key {
                    trigger: Trigger::Keysym(Keysym::Escape),
                    modifiers: Modifiers::empty(),
                },
                Action::CancelPickRegion,
            ),
            bind(
                Key {
                    trigger: Trigger::Keysym(Keysym::Return),
                    modifiers: Modifiers::empty(),
                },
                Action::ConfirmPickRegion,
            ),
            bind(
                Key {
                    trigger: Trigger::Keysym(Keysym::KP_Enter),
                    modifiers: Modifiers::empty(),
                },
                Action::ConfirmPickRegion,
            ),
        ];

        const SPACE: &[Bind] = &[bind(
            Key {
                trigger: Trigger::Keysym(Keysym::space),
                modifiers: Modifiers::empty(),
            },
            Action::ConfirmPickRegion,
        )];

        let Self::Open { button, .. } = self else {
            return [].iter().chain([].iter());
        };

        // Pressing Space while the button is down goes into origin moving rather than confirmation.
        let space: &[Bind] = if matches!(button, Button::Down { .. }) {
            &[]
        } else {
            SPACE
        };

        space.iter().chain(BINDS)
    }

    pub fn set_space_down(&mut self, down: bool) {
        if let Self::Open {
            selection,
            button:
                Button::Down {
                    move_state,
                    last_pos,
                    ..
                },
            ..
        } = self
        {
            if down {
                if move_state.is_none() {
                    *move_state = Some(MoveState {
                        pointer_offset: last_pos.1 - selection.1,
                        touch_slot: None,
                    });
                }
            } else {
                // Only clear if moving with Space.
                if let Some(MoveState {
                    touch_slot: None, ..
                }) = move_state
                {
                    *move_state = None;
                }
            }
        }
    }

    pub fn move_left(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, min(a.x, b.x));
        a.x -= delta;
        b.x -= delta;

        self.update_buffers();
    }

    pub fn move_right(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, data.size.w - max(a.x, b.x) - 1);
        a.x += delta;
        b.x += delta;

        self.update_buffers();
    }

    pub fn move_up(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, min(a.y, b.y));
        a.y -= delta;
        b.y -= delta;

        self.update_buffers();
    }

    pub fn move_down(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, data.size.h - max(a.y, b.y) - 1);
        a.y += delta;
        b.y += delta;

        self.update_buffers();
    }

    /// Moves the screenshot selection to a different output.
    ///
    /// This preserves the relative position while keeping logical size. It is (intentionally) very
    /// similar to how floating windows move across monitors, but with one difference: floating
    /// windows can go partially outside the view, while the screenshot selection cannot. So, we
    /// clamp it to new output bounds, trying to preserve the size if possible.
    pub fn move_to_output(&mut self, new_output: Output) {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            return;
        };

        let (current_output, current_a, current_b) = selection;

        if current_output == &new_output {
            return;
        }

        let Some(target_data) = output_data.get(&new_output) else {
            return;
        };

        let current_data = &output_data[current_output];

        let current_rect: Rectangle<_, Physical> = Rectangle::new(
            Point::from((current_a.x.min(current_b.x), current_a.y.min(current_b.y))),
            Size::from((
                (current_a.x.max(current_b.x) - current_a.x.min(current_b.x) + 1),
                (current_a.y.max(current_b.y) - current_a.y.min(current_b.y) + 1),
            )),
        );
        let current_rect = current_rect.to_f64();

        let rel_x = current_rect.loc.x / current_data.size.w as f64;
        let rel_y = current_rect.loc.y / current_data.size.h as f64;

        let factor = target_data.scale / current_data.scale;
        let mut new_width = (current_rect.size.w * factor).round() as i32;
        let mut new_height = (current_rect.size.h * factor).round() as i32;

        new_width = new_width.clamp(1, target_data.size.w);
        new_height = new_height.clamp(1, target_data.size.h);

        let new_x = (rel_x * target_data.size.w as f64).round() as i32;
        let new_y = (rel_y * target_data.size.h as f64).round() as i32;

        let max_x = target_data.size.w - new_width;
        let max_y = target_data.size.h - new_height;
        let new_x = new_x.clamp(0, max_x);
        let new_y = new_y.clamp(0, max_y);

        let new_rect = Rectangle::new(
            Point::from((new_x, new_y)),
            Size::from((new_width, new_height)),
        );

        *selection = (
            new_output,
            new_rect.loc,
            new_rect.loc + new_rect.size - Size::from((1, 1)),
        );

        self.update_buffers();
    }

    pub fn set_width(&mut self, change: SizeChange) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let available_size = f64::from(data.size.w);
        let current_size = max(a.x, b.x) + 1 - min(a.x, b.x);

        let new_size = match change {
            SizeChange::SetFixed(fixed) => to_physical_precise_round(data.scale, fixed),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
            SizeChange::AdjustFixed(delta) => {
                let delta = to_physical_precise_round(data.scale, delta);
                current_size.saturating_add(delta)
            }
            SizeChange::AdjustProportion(delta) => {
                let current_prop = f64::from(current_size) / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
        };
        let new_size = new_size.clamp(1, data.size.w - min(a.x, b.x)) - 1;
        a.x = min(a.x, b.x);
        b.x = a.x + new_size;

        self.update_buffers();
    }

    pub fn set_height(&mut self, change: SizeChange) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let available_size = f64::from(data.size.h);
        let current_size = max(a.y, b.y) + 1 - min(a.y, b.y);

        let new_size = match change {
            SizeChange::SetFixed(fixed) => to_physical_precise_round(data.scale, fixed),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
            SizeChange::AdjustFixed(delta) => {
                let delta = to_physical_precise_round(data.scale, delta);
                current_size.saturating_add(delta)
            }
            SizeChange::AdjustProportion(delta) => {
                let current_prop = f64::from(current_size) / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
        };
        let new_size = new_size.clamp(1, data.size.h - min(a.y, b.y)) - 1;
        a.y = min(a.y, b.y);
        b.y = a.y + new_size;

        self.update_buffers();
    }

    pub fn advance_animations(&mut self) {}

    pub fn are_animations_ongoing(&self) -> bool {
        let Self::Open { open_anim, .. } = self else {
            return false;
        };

        !open_anim.is_done()
    }

    fn update_buffers(&mut self) {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            panic!("region select UI must be open to update buffers");
        };

        let (selection_output, a, b) = selection;
        let mut rect = rect_from_corner_points(*a, *b);

        for (output, data) in output_data {
            let buffers = &mut data.buffers;
            let locations = &mut data.locations;
            let size = data.size;
            let scale = data.scale;

            if output == selection_output {
                // Check if the selection is still valid. If not, reset it back to default.
                if !Rectangle::from_size(size).contains_rect(rect) {
                    rect = Rectangle::new(
                        Point::from((size.w / 4, size.h / 4)),
                        Size::from((size.w / 2, size.h / 2)),
                    );
                    *a = rect.loc;
                    *b = rect.loc + rect.size - Size::from((1, 1));
                }

                let border = to_physical_precise_round(scale, SELECTION_BORDER);

                let resize = move |buffer: &mut SolidColorBuffer, w: i32, h: i32| {
                    let size = Size::<_, Physical>::from((w, h));
                    buffer.resize(size.to_f64().to_logical(scale));
                };

                resize(&mut buffers[0], rect.size.w + border * 2, border);
                resize(&mut buffers[1], rect.size.w + border * 2, border);
                resize(&mut buffers[2], border, rect.size.h);
                resize(&mut buffers[3], border, rect.size.h);

                resize(&mut buffers[4], size.w, rect.loc.y);
                resize(&mut buffers[5], size.w, size.h - rect.loc.y - rect.size.h);
                resize(&mut buffers[6], rect.loc.x, rect.size.h);
                resize(
                    &mut buffers[7],
                    size.w - rect.loc.x - rect.size.w,
                    rect.size.h,
                );

                locations[0] = Point::from((rect.loc.x - border, rect.loc.y - border));
                locations[1] = Point::from((rect.loc.x - border, rect.loc.y + rect.size.h));
                locations[2] = Point::from((rect.loc.x - border, rect.loc.y));
                locations[3] = Point::from((rect.loc.x + rect.size.w, rect.loc.y));

                locations[4] = Point::default();
                locations[5] = Point::from((0, rect.loc.y + rect.size.h));
                locations[6] = Point::from((0, rect.loc.y));
                locations[7] = Point::from((rect.loc.x + rect.size.w, rect.loc.y));
            } else {
                buffers[0].resize((0., 0.));
                buffers[1].resize((0., 0.));
                buffers[2].resize((0., 0.));
                buffers[3].resize((0., 0.));

                buffers[4].resize(size.to_f64().to_logical(data.scale));
                buffers[5].resize((0., 0.));
                buffers[6].resize((0., 0.));
                buffers[7].resize((0., 0.));
            }
        }
    }

    pub fn render_output(
        &self,
        output: &Output,
        push: &mut dyn FnMut(SolidColorRenderElement),
    ) {
        let _span = tracy_client::span!("RegionSelectUi::render_output");

        let Self::Open {
            output_data,
            open_anim,
            ..
        } = self
        else {
            panic!("region select UI must be open to render it");
        };

        let Some(output_data) = output_data.get(output) else {
            return;
        };

        let scale = output_data.scale;
        let progress = open_anim.clamped_value().clamp(0., 1.) as f32;

        for (buffer, loc) in zip(&output_data.buffers, &output_data.locations) {
            let elem = SolidColorRenderElement::from_buffer(
                buffer,
                loc.to_f64().to_logical(scale),
                progress,
                Kind::Unspecified,
            );
            push(elem);
        }
    }

    pub fn open_anim_progress(&self) -> f32 {
        if let Self::Open { open_anim, .. } = self {
            open_anim.clamped_value().clamp(0., 1.) as f32
        } else {
            1.0
        }
    }

    pub fn selection_output(&self) -> Option<&Output> {
        if let Self::Open {
            selection: (output, _, _),
            ..
        } = self
        {
            Some(output)
        } else {
            None
        }
    }

    pub fn selected_rect(&self) -> Option<(&Output, Rectangle<i32, Physical>)> {
        if let Self::Open {
            selection: (output, a, b),
            ..
        } = self
        {
            Some((output, rect_from_corner_points(*a, *b)))
        } else {
            None
        }
    }

    pub fn selected_logical_rect(
        &self,
        global_space: &Space<Window>,
    ) -> Option<(Rectangle<i32, smithay::utils::Logical>, Option<String>)> {
        let (output, rect) = self.selected_rect()?;
        let output_geo = global_space.output_geometry(output)?;
        let scale = output.current_scale().fractional_scale();

        let loc_logical = rect.loc.to_f64().to_logical(scale);
        let size_logical = rect.size.to_f64().to_logical(scale);

        let x = (output_geo.loc.x as f64 + loc_logical.x).round() as i32;
        let y = (output_geo.loc.y as f64 + loc_logical.y).round() as i32;
        let width = size_logical.w.round().max(1.0) as i32;
        let height = size_logical.h.round().max(1.0) as i32;

        Some((
            Rectangle::new(Point::from((x, y)), Size::from((width, height))),
            Some(output.name()),
        ))
    }

    pub fn output_size(&self, output: &Output) -> Option<(Size<i32, Physical>, f64, Transform)> {
        if let Self::Open { output_data, .. } = self {
            let data = output_data.get(output)?;
            Some((data.size, data.scale, data.transform))
        } else {
            None
        }
    }

    /// The pointer has moved to `point` relative to the current selection output.
    ///
    /// The point may be outside output bounds.
    pub fn pointer_motion(&mut self, point: Point<i32, Physical>, slot: Option<TouchSlot>) {
        let Self::Open {
            selection,
            output_data,
            button:
                Button::Down {
                    touch_slot,
                    last_pos,
                    move_state,
                },
            ..
        } = self
        else {
            return;
        };

        if *touch_slot != slot {
            return;
        }

        last_pos.1 = point;

        if let Some(move_state) = move_state {
            // The cursor offset is relative to selection.1.
            let delta = point - (selection.1 + move_state.pointer_offset);

            let desired = rect_from_corner_points(selection.1 + delta, selection.2 + delta);
            let bounds = Rectangle::from_size(output_data[&selection.0].size - desired.size);
            let clamped_loc = desired.loc.constrain(bounds);

            let delta = clamped_loc - rect_from_corner_points(selection.1, selection.2).loc;
            selection.1 += delta;
            selection.2 += delta;
        } else {
            let size = output_data[&selection.0].size;
            selection.2 = Point::new(point.x.clamp(0, size.w - 1), point.y.clamp(0, size.h - 1));
        }

        self.update_buffers();
    }

    pub fn pointer_down(
        &mut self,
        output: Output,
        point: Point<i32, Physical>,
        slot: Option<TouchSlot>,
        move_existing: bool,
    ) -> bool {
        let Self::Open {
            selection,
            output_data,
            button,
            ..
        } = self
        else {
            return false;
        };

        // Check if this is a second touch (different slot) while already dragging.
        if let Some(new_slot) = slot {
            if let Button::Down {
                move_state,
                last_pos,
                ..
            } = button
            {
                if move_state.is_none() {
                    *move_state = Some(MoveState {
                        pointer_offset: last_pos.1 - selection.1,
                        touch_slot: Some(new_slot),
                    });
                }
            }
        }

        if button.is_down() {
            return false;
        }

        if move_existing {
            if output != selection.0 {
                return false;
            }

            *button = Button::Down {
                touch_slot: slot,
                last_pos: (output, point),
                move_state: Some(MoveState {
                    pointer_offset: point - selection.1,
                    touch_slot: slot,
                }),
            };
            return true;
        }

        let Some(output_data) = output_data.get(&output) else {
            return false;
        };

        *button = Button::Down {
            touch_slot: slot,
            last_pos: (output.clone(), point),
            move_state: None,
        };

        let point = Point::new(
            point.x.clamp(0, output_data.size.w - 1),
            point.y.clamp(0, output_data.size.h - 1),
        );
        *selection = (output, point, point);

        self.update_buffers();

        true
    }

    pub fn pointer_up(&mut self, slot: Option<TouchSlot>) -> bool {
        let Self::Open {
            selection,
            output_data,
            button,
            ..
        } = self
        else {
            return false;
        };

        let Button::Down {
            touch_slot,
            ref mut move_state,
            ..
        } = *button
        else {
            return false;
        };

        if touch_slot != slot {
            // This is not our main touch, but it might be the move touch. If so, stop the move.
            if let Some(state) = move_state {
                if state.touch_slot.is_some_and(|m_slot| Some(m_slot) == slot) {
                    *move_state = None;
                }
            };

            return false;
        }

        *button = Button::Up;

        // Check if the resulting selection is zero-sized, and try to come up with a small
        // default rectangle.
        let (output, a, b) = selection;
        let mut rect = rect_from_corner_points(*a, *b);
        if rect.size.is_empty() || rect.size == Size::from((1, 1)) {
            let data = &output_data[output];
            rect = Rectangle::new(
                Point::from((rect.loc.x - 16, rect.loc.y - 16)),
                Size::from((32, 32)),
            )
            .intersection(Rectangle::from_size(data.size))
            .unwrap_or_default();
            *a = rect.loc;
            *b = rect.loc + rect.size - Size::from((1, 1));
        }

        self.update_buffers();

        true
    }
}

pub fn rect_from_corner_points(
    a: Point<i32, Physical>,
    b: Point<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let x1 = min(a.x, b.x);
    let y1 = min(a.y, b.y);
    let x2 = max(a.x, b.x);
    let y2 = max(a.y, b.y);
    // We're adding + 1 because the pointer is clamped to output size - 1, so to get the full
    // screen worth of selection we must add back that + 1.
    Rectangle::from_extremities((x1, y1), (x2 + 1, y2 + 1))
}
