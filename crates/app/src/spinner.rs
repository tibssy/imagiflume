//! Spinner that keeps animating across hide/show cycles.
//!
//! GtkSpinner pauses its internal SVG animation when unmapped in GTK 4.24, but
//! `ensure_paintable()` returns early on remap before calling `gtk_svg_play()`,
//! so a spinner in a hideable sidebar freezes. A tick callback is registered
//! for the widget's lifetime instead and resumes whenever it is mapped.

use std::cell::{Cell, RefCell};
use std::f64::consts::TAU;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, glib};

pub(crate) struct Spinner {
    pub widget: gtk::DrawingArea,
    spinning: Rc<Cell<bool>>,
    angle: Rc<Cell<f64>>,
    tick: Rc<RefCell<Option<gtk::TickCallbackId>>>,
}

impl Spinner {
    pub fn new(size: i32) -> Self {
        let widget = gtk::DrawingArea::new();
        widget.set_size_request(size, size);
        widget.set_content_width(size);
        widget.set_content_height(size);
        widget.set_halign(gtk::Align::Center);
        widget.set_valign(gtk::Align::Center);

        let spinning = Rc::new(Cell::new(false));
        let angle = Rc::new(Cell::new(0.0));
        {
            let spinning = spinning.clone();
            let angle = angle.clone();
            widget.set_draw_func(move |area, cr, width, height| {
                if spinning.get() {
                    draw(cr, area.color(), width, height, angle.get());
                }
            });
        }

        Self {
            widget,
            spinning,
            angle,
            tick: Rc::new(RefCell::new(None)),
        }
    }

    pub fn start(&self) {
        self.spinning.set(true);
        self.widget.queue_draw();
        let mut tick = self.tick.borrow_mut();
        if tick.is_some() {
            return;
        }
        let spinning = self.spinning.clone();
        let angle = self.angle.clone();
        let slot = self.tick.clone();
        *tick = Some(self.widget.add_tick_callback(move |area, clock| {
            if !spinning.get() {
                slot.borrow_mut().take();
                return glib::ControlFlow::Break;
            }
            angle.set((clock.frame_time() as f64 / 1_000_000_000.0) * TAU);
            area.queue_draw();
            glib::ControlFlow::Continue
        }));
    }

    pub fn stop(&self) {
        self.spinning.set(false);
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
        self.widget.queue_draw();
    }

    pub fn set_visible(&self, visible: bool) {
        self.widget.set_visible(visible);
    }
}

fn draw(cr: &cairo::Context, color: gtk::gdk::RGBA, width: i32, height: i32, angle: f64) {
    let red = color.red() as f64;
    let green = color.green() as f64;
    let blue = color.blue() as f64;
    let side = width.min(height) as f64;
    let radius = ((side - 4.0) / 2.0).max(1.0);
    let center_x = width as f64 / 2.0;
    let center_y = height as f64 / 2.0;

    cr.set_line_width((side / 8.0).clamp(1.5, 2.5));
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_source_rgba(red, green, blue, 0.9);
    cr.arc(center_x, center_y, radius, angle, angle + 4.4);
    cr.stroke().ok();
}
