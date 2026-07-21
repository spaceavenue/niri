use std::cell::RefCell;
use std::cmp::max;
use std::collections::HashMap;
use std::f64::consts::TAU;
use std::rc::Rc;

use anyhow::Context;
use arrayvec::ArrayVec;
use niri_config::{Action, Config};
use niri_ipc::SizeChange;
use pango::{Alignment, FontDescription};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::TouchSlot;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ExportMem, Texture as _};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::output::Output;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::animation::Clock;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::solid_color::SolidColorRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_texture, RenderTarget};
pub use crate::ui::region_select::rect_from_corner_points;
use crate::ui::region_select::RegionSelectUi;
use crate::utils::to_physical_precise_round;

const PADDING: i32 = 8;
const RADIUS: i32 = 16;
const FONT: &str = "sans 14px";
const BORDER: i32 = 4;
const TEXT_HIDE_P: &str =
    "Press <span face='mono' bgcolor='#2C2C2C'> Space </span> to save the screenshot.\n\
     Press <span face='mono' bgcolor='#2C2C2C'> P </span> to hide the pointer.";
const TEXT_SHOW_P: &str =
    "Press <span face='mono' bgcolor='#2C2C2C'> Space </span> to save the screenshot.\n\
     Press <span face='mono' bgcolor='#2C2C2C'> P </span> to show the pointer.";

// Ideally the screenshot UI should support cross-output selections. However, that poses some
// technical challenges when the outputs have different scales and such. So, this implementation
// allows only single-output selections for now.
//
// As a consequence of this, selection coordinates are in output-local coordinate space.
pub struct ScreenshotUi {
    region_select: RegionSelectUi,
    state: State,
}

#[allow(clippy::large_enum_variant)]
enum State {
    Closed,
    Open {
        output_data: HashMap<Output, OutputData>,
        button: Button,
        show_pointer: bool,
        path: Option<String>,
        stdout_tx: Option<async_channel::Sender<Vec<u8>>>,
    },
}

pub enum Button {
    Up,
    Down {
        touch_slot: Option<TouchSlot>,
        on_capture_button: bool,
        last_pos: (Output, Point<i32, Physical>),
    },
}

pub struct OutputData {
    scale: f64,
    // Output, screencast, screen capture.
    screenshot: [OutputScreenshot; 3],
    panel: Option<(TextureBuffer<GlesTexture>, TextureBuffer<GlesTexture>)>,
}

pub struct OutputScreenshot {
    texture: GlesTexture,
    buffer: PrimaryGpuTextureRenderElement,
    pointer: Option<PrimaryGpuTextureRenderElement>,
}

niri_render_elements! {
    ScreenshotUiRenderElement => {
        Screenshot = PrimaryGpuTextureRenderElement,
        SolidColor = SolidColorRenderElement,
    }
}

impl Button {
    fn is_down(&self) -> bool {
        matches!(self, Self::Down { .. })
    }

    fn is_dragging_selection(&self) -> bool {
        matches!(
            self,
            Self::Down {
                on_capture_button: false,
                ..
            }
        )
    }
}

impl ScreenshotUi {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            region_select: RegionSelectUi::new(clock, config),
            state: State::Closed,
        }
    }

    pub fn open(
        &mut self,
        renderer: &mut GlesRenderer,
        // Output, screencast, screen capture.
        screenshots: HashMap<Output, [OutputScreenshot; 3]>,
        default_output: Output,
        show_pointer: bool,
        path: Option<String>,
        stdout_tx: Option<async_channel::Sender<Vec<u8>>>,
    ) -> bool {
        if screenshots.is_empty() {
            return false;
        }

        let State::Closed = self.state else {
            return false;
        };

        if !self
            .region_select
            .open(screenshots.keys().cloned(), default_output)
        {
            return false;
        }

        let output_data = screenshots
            .into_iter()
            .map(|(output, screenshot)| {
                let scale = output.current_scale().fractional_scale();

                let mut render_panel_ = |text| {
                    render_panel(renderer, scale, text)
                        .map_err(|err| warn!("error rendering help panel: {err:?}"))
                        .ok()
                };
                let panel_show = render_panel_(TEXT_SHOW_P);
                let panel_hide = render_panel_(TEXT_HIDE_P);
                let panel = Option::zip(panel_show, panel_hide);

                let data = OutputData {
                    scale,
                    screenshot,
                    panel,
                };
                (output, data)
            })
            .collect();

        self.state = State::Open {
            output_data,
            button: Button::Up,
            show_pointer,
            path,
            stdout_tx,
        };

        true
    }

    pub fn close(&mut self) -> bool {
        let State::Open { .. } = self.state else {
            return false;
        };

        self.region_select.close();
        self.state = State::Closed;

        true
    }

    pub fn toggle_pointer(&mut self) {
        if let State::Open { show_pointer, .. } = &mut self.state {
            *show_pointer = !*show_pointer;
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open { .. })
    }

    pub fn take_path(&mut self) -> Option<Option<String>> {
        let State::Open { path, .. } = &mut self.state else {
            return None;
        };
        Some(path.take())
    }

    pub fn take_stdout_tx(&mut self) -> Option<Option<async_channel::Sender<Vec<u8>>>> {
        let State::Open { stdout_tx, .. } = &mut self.state else {
            return None;
        };
        Some(stdout_tx.take())
    }

    pub fn set_space_down(&mut self, down: bool) {
        if self.is_open() {
            self.region_select.set_space_down(down);
        }
    }

    pub fn move_left(&mut self) {
        if self.is_open() {
            self.region_select.move_left();
        }
    }

    pub fn move_right(&mut self) {
        if self.is_open() {
            self.region_select.move_right();
        }
    }

    pub fn move_up(&mut self) {
        if self.is_open() {
            self.region_select.move_up();
        }
    }

    pub fn move_down(&mut self) {
        if self.is_open() {
            self.region_select.move_down();
        }
    }

    /// Moves the screenshot selection to a different output.
    ///
    /// This preserves the relative position while keeping logical size. It is (intentionally) very
    /// similar to how floating windows move across monitors, but with one difference: floating
    /// windows can go partially outside the view, while the screenshot selection cannot. So, we
    /// clamp it to new output bounds, trying to preserve the size if possible.
    pub fn move_to_output(&mut self, new_output: Output) {
        if self.is_open() {
            self.region_select.move_to_output(new_output);
        }
    }

    pub fn set_width(&mut self, change: SizeChange) {
        if self.is_open() {
            self.region_select.set_width(change);
        }
    }

    pub fn set_height(&mut self, change: SizeChange) {
        if self.is_open() {
            self.region_select.set_height(change);
        }
    }

    pub fn advance_animations(&mut self) {
        if self.is_open() {
            self.region_select.advance_animations();
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.is_open() && self.region_select.are_animations_ongoing()
    }

    pub fn render_output(
        &self,
        output: &Output,
        target: RenderTarget,
        push: &mut dyn FnMut(ScreenshotUiRenderElement),
    ) {
        let _span = tracy_client::span!("ScreenshotUi::render_output");

        let State::Open {
            output_data,
            show_pointer,
            button,
            ..
        } = &self.state
        else {
            panic!("screenshot UI must be open to render it");
        };

        let Some(output_data) = output_data.get(output) else {
            return;
        };

        let scale = output_data.scale;
        let progress = self.region_select.open_anim_progress();

        // The help panel goes on top.
        if let Some((show, hide)) = &output_data.panel {
            let buffer = if *show_pointer { hide } else { show };
            let alpha = if button.is_dragging_selection() {
                0.3
            } else {
                0.9
            };
            if let Some((output_size, _, _)) = self.region_select.output_size(output) {
                let location = panel_location(scale, output_size, buffer.texture().size())
                    .to_f64()
                    .to_logical(scale);

                let elem =
                    PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
                        buffer.clone(),
                        location,
                        alpha * progress,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                push(elem.into());
            }
        }

        self.region_select.render_output(output, &mut |elem| {
            push(elem.into());
        });

        // The screenshot itself goes last.
        let index = match target {
            RenderTarget::Output => 0,
            RenderTarget::Screencast => 1,
            RenderTarget::ScreenCapture => 2,
        };
        let screenshot = &output_data.screenshot[index];

        if *show_pointer {
            if let Some(pointer) = screenshot.pointer.clone() {
                push(pointer.into());
            }
        }
        push(screenshot.buffer.clone().into());
    }

    pub fn capture(
        &self,
        renderer: &mut GlesRenderer,
    ) -> anyhow::Result<(Size<i32, Physical>, Vec<u8>)> {
        let _span = tracy_client::span!("ScreenshotUi::capture");

        let State::Open {
            output_data,
            show_pointer,
            ..
        } = &self.state
        else {
            panic!("screenshot UI must be open to capture");
        };

        let (output, rect) = self
            .region_select
            .selected_rect()
            .context("no active selection to capture")?;
        let data = &output_data[output];

        let screenshot = &data.screenshot[0];

        // Composite the pointer on top if needed.
        let mut tex_rect = None;
        if *show_pointer {
            if let Some(pointer) = screenshot.pointer.clone() {
                let scale = pointer.0.buffer().texture_scale();
                let offset = rect.loc.upscale(-1);

                let mut elements = ArrayVec::<_, 2>::new();
                elements.push(pointer);
                elements.push(screenshot.buffer.clone());
                let elements = elements.iter().rev().map(|elem| {
                    RelocateRenderElement::from_element(elem, offset, Relocate::Relative)
                });

                let res = render_to_texture(
                    renderer,
                    rect.size,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements,
                );
                match res {
                    Ok((texture, _)) => {
                        tex_rect = Some((texture, Rectangle::from_size(rect.size)));
                    }
                    Err(err) => {
                        warn!("error compositing pointer onto screenshot: {err:?}");
                    }
                }
            }
        }

        let (texture, rect) = tex_rect.unwrap_or_else(|| (screenshot.texture.clone(), rect));
        // The size doesn't actually matter because we're not transforming anything.
        let buf_rect = rect
            .to_logical(1)
            .to_buffer(1, Transform::Normal, &Size::from((1, 1)));

        let mapping = renderer
            .copy_texture(&texture, buf_rect, Fourcc::Abgr8888)
            .context("error copying texture")?;
        let copy = renderer
            .map_texture(&mapping)
            .context("error mapping texture")?;

        Ok((rect.size, copy.to_vec()))
    }

    pub fn action(&self, raw: Keysym, mods: ModifiersState) -> Option<Action> {
        let State::Open { button, .. } = &self.state else {
            return None;
        };

        // Pressing Space while the button is down goes into origin moving rather than capture.
        if matches!(button, Button::Down { .. }) && raw == Keysym::space {
            return None;
        }

        action(raw, mods)
    }

    pub fn selection_output(&self) -> Option<&Output> {
        if self.is_open() {
            self.region_select.selection_output()
        } else {
            None
        }
    }

    pub fn output_size(&self, output: &Output) -> Option<(Size<i32, Physical>, f64, Transform)> {
        if self.is_open() {
            self.region_select.output_size(output)
        } else {
            None
        }
    }

    /// The pointer has moved to `point` relative to the current selection output.
    ///
    /// The point may be outside output bounds.
    pub fn pointer_motion(&mut self, point: Point<i32, Physical>, slot: Option<TouchSlot>) {
        let State::Open { button, .. } = &mut self.state else {
            return;
        };

        if let Button::Down {
            touch_slot,
            on_capture_button,
            last_pos,
        } = button
        {
            if *touch_slot != slot {
                return;
            }
            last_pos.1 = point;
            if *on_capture_button {
                return;
            }
        }

        self.region_select.pointer_motion(point, slot);
    }

    pub fn pointer_down(
        &mut self,
        output: Output,
        point: Point<i32, Physical>,
        slot: Option<TouchSlot>,
        move_existing: bool,
    ) -> bool {
        let State::Open {
            output_data,
            show_pointer,
            button,
            ..
        } = &mut self.state
        else {
            return false;
        };

        // Check if this is a second touch (different slot) while already dragging.
        if button.is_down() {
            if matches!(
                button,
                Button::Down {
                    on_capture_button: false,
                    ..
                }
            ) {
                self.region_select
                    .pointer_down(output, point, slot, move_existing);
            }
            return false;
        }

        if move_existing {
            *button = Button::Down {
                touch_slot: slot,
                on_capture_button: false,
                last_pos: (output.clone(), point),
            };
            return self.region_select.pointer_down(output, point, slot, true);
        }

        let Some(data) = output_data.get(&output) else {
            return false;
        };

        if let Some((show, hide)) = &data.panel {
            let buffer = if *show_pointer { hide } else { show };
            let panel_size = buffer.texture().size();
            if let Some((output_size, _, _)) = self.region_select.output_size(&output) {
                let location = panel_location(data.scale, output_size, panel_size);

                if is_within_capture_button(data.scale, panel_size, point - location) {
                    *button = Button::Down {
                        touch_slot: slot,
                        on_capture_button: true,
                        last_pos: (output, point),
                    };
                    return false;
                }
            }
        }

        *button = Button::Down {
            touch_slot: slot,
            on_capture_button: false,
            last_pos: (output.clone(), point),
        };

        self.region_select.pointer_down(output, point, slot, false)
    }

    pub fn pointer_up(&mut self, slot: Option<TouchSlot>) -> Option<bool> {
        let State::Open {
            output_data,
            button,
            show_pointer,
            ..
        } = &mut self.state
        else {
            return None;
        };

        let Button::Down {
            touch_slot,
            on_capture_button,
            ref last_pos,
        } = *button
        else {
            return None;
        };

        if touch_slot != slot {
            // Forward to region_select so it can check if it was a second touch move slot.
            self.region_select.pointer_up(slot);
            return None;
        }

        let last_pos = last_pos.clone();
        *button = Button::Up;

        // Check if we released still on the capture button.
        if on_capture_button {
            let (output, point) = last_pos;

            #[allow(clippy::question_mark)]
            let Some(data) = output_data.get(&output) else {
                return None;
            };

            if let Some((show, hide)) = &data.panel {
                let buffer = if *show_pointer { hide } else { show };
                let panel_size = buffer.texture().size();
                if let Some((output_size, _, _)) = self.region_select.output_size(&output) {
                    let location = panel_location(data.scale, output_size, panel_size);

                    if is_within_capture_button(data.scale, panel_size, point - location) {
                        return Some(true);
                    }
                }
            }
        }

        self.region_select.pointer_up(slot);
        Some(false)
    }
}

impl OutputScreenshot {
    pub fn from_textures(
        renderer: &mut GlesRenderer,
        scale: Scale<f64>,
        texture: GlesTexture,
        pointer: Option<(GlesTexture, Rectangle<i32, Physical>)>,
    ) -> Self {
        let buffer = PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
            TextureBuffer::from_texture(
                renderer,
                texture.clone(),
                scale,
                Transform::Normal,
                Vec::new(),
            ),
            (0., 0.),
            1.,
            None,
            None,
            Kind::Unspecified,
        ));

        let pointer = pointer.map(|(texture, geo)| {
            PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
                TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                ),
                geo.to_f64().to_logical(scale).loc,
                1.,
                None,
                None,
                Kind::Unspecified,
            ))
        });

        Self {
            texture,
            buffer,
            pointer,
        }
    }
}

fn action(raw: Keysym, mods: ModifiersState) -> Option<Action> {
    if raw == Keysym::Escape {
        return Some(Action::CancelScreenshot);
    }

    if mods.alt || mods.shift {
        return None;
    }

    if !mods.ctrl && (raw == Keysym::space || raw == Keysym::Return) {
        return Some(Action::ConfirmScreenshot {
            write_to_disk: true,
        });
    }
    if mods.ctrl && raw == Keysym::c {
        return Some(Action::ConfirmScreenshot {
            write_to_disk: false,
        });
    }

    if !mods.ctrl && raw == Keysym::p {
        return Some(Action::ScreenshotTogglePointer);
    }

    None
}

fn panel_location(
    scale: f64,
    output_size: Size<i32, Physical>,
    panel_size: Size<i32, Buffer>,
) -> Point<i32, Physical> {
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let x = max(0, (output_size.w - panel_size.w) / 2);
    let y = max(0, output_size.h - panel_size.h - padding * 2);
    Point::from((x, y))
}

fn is_within_capture_button(
    scale: f64,
    panel_size: Size<i32, Buffer>,
    pos_within_panel: Point<i32, Physical>,
) -> bool {
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let radius = to_physical_precise_round::<i32>(scale, RADIUS) - 2;

    let xc = padding + radius;
    let yc = panel_size.h / 2;
    let pos = pos_within_panel;

    (pos.x - xc) * (pos.x - xc) + (pos.y - yc) * (pos.y - yc) <= radius * radius
}

fn render_panel(
    renderer: &mut GlesRenderer,
    scale: f64,
    text: &str,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let _span = tracy_client::span!("screenshot_ui::render_panel");

    let padding: i32 = to_physical_precise_round(scale, PADDING);
    // Keep the border width even to avoid blurry edges.
    let border_width = (f64::from(BORDER) / 2. * scale).round() * 2.;
    let half_border_width = (border_width / 2.) as i32;
    let radius: i32 = to_physical_precise_round(scale, RADIUS);
    let circle_stroke: f64 = to_physical_precise_round(scale, 2.);

    // Add 2 px of spacing to separate the backgrounds of the "Space" and "P" keys.
    let spacing = to_physical_precise_round::<i32>(scale, 2) * 1024;

    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_alignment(Alignment::Left);
    layout.set_markup(text);
    layout.set_spacing(spacing);

    let (mut width, mut height) = layout.pixel_size();

    width += padding + radius * 2 + padding - half_border_width + padding;
    height = max(height, radius * 2);
    height += padding * 2;

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.paint()?;

    let padding = f64::from(padding);
    let half_border_width = f64::from(half_border_width);
    let r = f64::from(radius);

    let yc = f64::from(height / 2);

    cr.new_sub_path();
    cr.arc(padding + r, yc, r, 0., TAU);
    cr.set_source_rgb(1., 1., 1.);
    cr.fill()?;

    cr.new_sub_path();
    cr.arc(padding + r, yc, r - circle_stroke, 0., TAU);
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.fill()?;

    cr.new_sub_path();
    cr.arc(padding + r, yc, r - circle_stroke * 2., 0., TAU);
    cr.set_source_rgb(1., 1., 1.);
    cr.fill()?;

    cr.move_to(padding + r * 2. + padding - half_border_width, padding);

    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_alignment(Alignment::Left);
    layout.set_markup(text);
    layout.set_spacing(spacing);

    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &layout);

    cr.move_to(0., 0.);
    cr.line_to(width.into(), 0.);
    cr.line_to(width.into(), height.into());
    cr.line_to(0., height.into());
    cr.line_to(0., 0.);
    cr.set_source_rgb(0.3, 0.3, 0.3);
    cr.set_line_width(border_width);
    cr.stroke()?;
    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
}
