use molex::entity::molecule::id::EntityId;

use crate::app::App;
#[cfg(not(target_arch = "wasm32"))]
use foldit_gui::DirtyFlags;

impl App {
    pub fn handle_app_command(&mut self, command: foldit_gui::AppCommand) {
        use foldit_gui::AppCommand;

        // These commands need no engine, so they dispatch ahead of the
        // `if self.harness.engine.is_none()` guard below.
        if let AppCommand::History { cmd } = command {
            if self.harness.engine.is_none() {
                return;
            }
            let aborted = self.store.apply_history_command(&cmd);
            #[cfg(not(target_arch = "wasm32"))]
            for rid in aborted {
                let _ = self.scores.remove_target(rid);
            }
            #[cfg(target_arch = "wasm32")]
            let _ = aborted;
            return;
        }

        if let AppCommand::AdvanceBubble { back } = command {
            self.advance_bubble(back);
            return;
        }

        if let AppCommand::SetFocus { entity_id } = command {
            let focus = entity_id.map_or(viso::Focus::All, |raw| {
                viso::Focus::Entity(EntityId::from_raw(raw))
            });
            self.store.set_focus(focus);
            return;
        }

        // Per-entity appearance is authoritative on the session; the render
        // projector pushes it into the engine working copy on the emitted
        // `EntityAppearanceChanged`.
        if let AppCommand::SetEntityAppearance {
            entity_id,
            field,
            value,
        } = command
        {
            self.store
                .set_entity_appearance_field(EntityId::from_raw(entity_id), &field, &value);
            return;
        }

        if let AppCommand::ClearEntityAppearance { entity_id } = command {
            self.store
                .clear_entity_appearance(EntityId::from_raw(entity_id));
            return;
        }

        if matches!(command, AppCommand::CloseSegment) {
            self.gui.close_segment();
            return;
        }

        if let AppCommand::SetPanelVisible { panel, visible } = command {
            self.gui.set_panel_visible(panel, visible);
            return;
        }
        if let AppCommand::SetPanelPosition { panel, x, y } = command {
            self.gui.set_panel_position(panel, x, y);
            return;
        }
        if let AppCommand::SetActionPickerOpen { op_id } = command {
            self.gui.set_action_picker_open(op_id);
            return;
        }

        if let AppCommand::SetHintsVisible { visible } = command {
            self.gui.set_hints_visible(visible);
            return;
        }
        if let AppCommand::SetFullscreen { value } = command {
            self.gui.set_fullscreen(value);
            return;
        }

        if matches!(command, AppCommand::ClearProgress) {
            self.gui.clear_progress();
            return;
        }

        if self.harness.engine.is_none() {
            return;
        }

        self.handle_engine_command(command);
    }

    // Dispatch the engine-dependent commands. Reached only after the
    // engine-presence guard, so an engine is present.
    fn handle_engine_command(&mut self, command: foldit_gui::AppCommand) {
        use foldit_gui::AppCommand;

        match command {
            AppCommand::LoadStructure { path } => self.handle_load_structure(&path),
            AppCommand::LoadPuzzle { puzzle_id } => self.handle_load_puzzle(puzzle_id),
            AppCommand::LoadPuzzleDir { path } => self.handle_load_puzzle_dir(&path),
            AppCommand::SetViewOptions { options } => {
                // A manual edit: store the faithful (sparse) options, clear the
                // active preset (manually-set options no longer match a named
                // preset), and latch the player-touched flag. Deserialize the
                // inbound dense options into viso's type, then re-serialize the
                // faithful form so the frontend holds the round-trip source.
                match serde_json::from_value::<viso::options::VisoOptions>(options) {
                    Ok(opts) => {
                        let faithful = serde_json::to_value(&opts).unwrap_or_default();
                        if self.gui.set_view_manual(faithful) {
                            self.store.note_view_options_changed();
                        }
                    }
                    Err(e) => log::error!("Failed to deserialize view options: {e}"),
                }
            }
            AppCommand::LoadViewPreset { name } => {
                // An explicit player preset pick: latch the touched flag (so it
                // persists across later loads) and apply the preset now.
                self.gui.set_view_touched(true);
                #[cfg(not(target_arch = "wasm32"))]
                self.apply_view_preset_to_session(&name);
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            AppCommand::SaveViewPreset { name } => {
                // Writes to the preset *library* on disk; it does not change
                // the active view options, only the available-presets list.
                // No `SessionUpdate` carries a disk-library change, so refresh
                // just that list onto the frontend at-site (the same read the
                // VIEW arm of the GUI consumer does) rather than re-pushing
                // the whole VIEW section.
                #[cfg(not(target_arch = "wasm32"))]
                // Own the dir so the `&self.host` borrow is released before
                // the disjoint `&mut self.harness.engine` / `&mut self.gui`
                // borrows below.
                if let Some(dir) = self
                    .host
                    .view_presets_dir()
                    .map(std::path::Path::to_path_buf)
                {
                    if let Some(engine) = self.harness.engine.as_mut() {
                        engine.save_preset(&name, &dir);
                    }
                    self.gui.view.available_presets =
                        viso::options::VisoOptions::list_presets(&dir);
                    self.gui.mark_dirty(DirtyFlags::VIEW);
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            AppCommand::History { .. }
            | AppCommand::AdvanceBubble { .. }
            | AppCommand::SetFocus { .. }
            | AppCommand::SetEntityAppearance { .. }
            | AppCommand::ClearEntityAppearance { .. }
            | AppCommand::CloseSegment
            | AppCommand::SetPanelVisible { .. }
            | AppCommand::SetPanelPosition { .. }
            | AppCommand::SetActionPickerOpen { .. }
            | AppCommand::SetHintsVisible { .. }
            | AppCommand::SetFullscreen { .. }
            | AppCommand::ClearProgress => {
                // Handled in the early-return block above. The match is
                // exhaustive over `AppCommand`: a new variant
                // without a handler is a compile error.
            }
        }
    }

    /// Step the tutorial-bubble cursor on the session. The cursor lives on
    /// `Session` now; this forwards and the emitted `BubbleChanged` is
    /// turned into `TEXT_BUBBLE` dirty by the tick, which re-pushes the new
    /// head. Forward saturates one past the end (the GUI then clears);
    /// back saturates at 0.
    fn advance_bubble(&mut self, back: bool) {
        self.store.advance_bubble(back);
    }
}
