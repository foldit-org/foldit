use crate::AppState;
use foldit_rs::frontend::{ActionId, DirtyFlags, ParameterizedAction, ViewportInput};
use tauri::State;

#[tauri::command]
pub fn viewport_input(input: ViewportInput, state: State<'_, AppState>) {
    let mut app = state.app.lock().unwrap();
    let engine = match app.engine.as_mut() {
        Some(e) => e,
        None => return,
    };

    match input {
        ViewportInput::PointerDown { x, y, button } => {
            let winit_button = match button {
                0 => winit::event::MouseButton::Left,
                2 => winit::event::MouseButton::Right,
                1 => winit::event::MouseButton::Middle,
                _ => return,
            };
            engine.handle_mouse_button(winit_button, true);
            engine.handle_mouse_position(x, y);
        }
        ViewportInput::PointerUp { x, y, button } => {
            let winit_button = match button {
                0 => winit::event::MouseButton::Left,
                2 => winit::event::MouseButton::Right,
                1 => winit::event::MouseButton::Middle,
                _ => return,
            };
            engine.handle_mouse_button(winit_button, false);
            engine.handle_mouse_up();
            engine.handle_mouse_position(x, y);
        }
        ViewportInput::PointerMove { x, y, dx, dy } => {
            engine.handle_mouse_move(dx, dy);
            engine.handle_mouse_position(x, y);
        }
        ViewportInput::Scroll { delta } => {
            engine.handle_mouse_wheel(delta);
        }
        ViewportInput::Key { code, pressed } => {
            if pressed {
                app.handle_key_by_name(&code);
            }
        }
        ViewportInput::Resize { width, height } => {
            engine.resize(width, height);
        }
    }

    // Mark UI dirty for any viewport input (FPS, hover state, etc.)
    let mut frontend = state.frontend.lock().unwrap();
    frontend.mark_dirty(DirtyFlags::UI);
}

#[tauri::command]
pub fn trigger_action(action: ActionId, state: State<'_, AppState>) {
    let mut app = state.app.lock().unwrap();

    match action {
        ActionId::ToggleWiggle => app.handle_key(winit::keyboard::KeyCode::KeyW),
        ActionId::ToggleShake => app.handle_key(winit::keyboard::KeyCode::KeyS),
        ActionId::RunPrediction => app.handle_key(winit::keyboard::KeyCode::KeyP),
        ActionId::RunMPNN => app.handle_key(winit::keyboard::KeyCode::KeyM),
        ActionId::RunDiffusion => app.handle_key(winit::keyboard::KeyCode::KeyR),
        ActionId::ToggleViewMode => app.handle_key(winit::keyboard::KeyCode::KeyV),
        ActionId::ToggleBackboneQuality => app.handle_key(winit::keyboard::KeyCode::KeyQ),
        ActionId::ToggleDesignedStructures => app.handle_key(winit::keyboard::KeyCode::KeyH),
        ActionId::CycleFocus => app.handle_key(winit::keyboard::KeyCode::Tab),
        ActionId::RemoveStructure => app.handle_key(winit::keyboard::KeyCode::Delete),
        ActionId::Cancel => app.handle_key(winit::keyboard::KeyCode::Escape),
        ActionId::Undo | ActionId::Redo => {
            log::warn!("Undo/Redo not yet implemented");
        }
    }

    // Actions can affect multiple sections
    let mut frontend = state.frontend.lock().unwrap();
    frontend.mark_dirty(DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::UI);
}

#[tauri::command]
pub fn parameterized_action(action: ParameterizedAction, state: State<'_, AppState>) {
    let mut app = state.app.lock().unwrap();

    match action {
        ParameterizedAction::LoadStructure { path } => {
            app.handle_load_structure(&path);
            let mut frontend = state.frontend.lock().unwrap();
            frontend.mark_dirty(DirtyFlags::LOADING | DirtyFlags::SCORE | DirtyFlags::SELECTION);
        }
        ParameterizedAction::CreateBand { .. } => {
            log::info!("CreateBand via IPC not yet wired");
        }
        ParameterizedAction::RemoveBand { .. } => {
            log::info!("RemoveBand via IPC not yet wired");
        }
        ParameterizedAction::SetViewOption { .. } => {
            log::info!("SetViewOption via IPC not yet wired");
            let mut frontend = state.frontend.lock().unwrap();
            frontend.mark_dirty(DirtyFlags::VIEW);
        }
    }
}
