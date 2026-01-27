//! IPC socket server.

use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use catacomb_ipc::{AppIdMatcher, CliToggle, IpcMessage, Keysym, WindowScale};
use smithay::backend::input::KeyState;
use smithay::input::keyboard::keysyms;
use smithay::input::keyboard::XkbConfig;
use smithay::reexports::calloop::LoopHandle;
use tracing::{error, warn};
use libc;

use crate::catacomb::Catacomb;
use crate::config::{GestureBinding, GestureBindingAction, KeyBinding};
use crate::socket::SocketSource;

/// Create an IPC socket.
pub fn spawn_ipc_socket(
    event_loop: &LoopHandle<'static, Catacomb>,
    socket_name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let socket_path = catacomb_ipc::socket_path(socket_name);

    // Try to delete the socket if it exists already.
    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }

    // Spawn unix socket event source.
    let listener = UnixListener::bind(&socket_path)?;
    let socket = SocketSource::new(listener)?;

    // Add source to calloop loop.
    let mut message_buffer = String::new();
    event_loop.insert_source(socket, move |stream, _, catacomb| {
        handle_message(&mut message_buffer, stream, catacomb);
    })?;

    Ok(socket_path)
}

/// Handle IPC socket messages.
fn handle_message(buffer: &mut String, mut stream: UnixStream, catacomb: &mut Catacomb) {
    buffer.clear();

    // Read new content to buffer.
    let mut reader = BufReader::new(&stream);
    if let Ok(0) | Err(_) = reader.read_line(buffer) {
        return;
    }

    // Read pending events on socket.
    let message: IpcMessage = match serde_json::from_str(buffer) {
        Ok(message) => message,
        Err(_) => return,
    };

    // Handle IPC events.
    match message {
        IpcMessage::Orientation { unlock: true, .. } => catacomb.unlock_orientation(),
        IpcMessage::Orientation { lock: orientation, .. } => catacomb.lock_orientation(orientation),
        IpcMessage::Focus { app_id } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: focus has invalid App ID regex: {err}");
                    return;
                },
            };

            catacomb.focus_app(app_id);
        },
        IpcMessage::ExecOrFocus { command, app_id_hint, card_id } => {
            // Try to focus existing window by strict App ID first.
            if let Some(app_id) = app_id_hint {
                match AppIdMatcher::try_from(app_id) {
                    Ok(app) => {
                        if catacomb.windows.focus_app(app) {
                            catacomb.unstall();
                            return;
                        }
                    },
                    Err(err) => {
                        warn!("ignoring invalid ipc message: ExecOrFocus has invalid App ID regex: {err}");
                    },
                }
            }
            if let Some(c) = card_id.as_ref() {
                if catacomb.windows.focus_mapped_card(c) {
                    catacomb.unstall();
                    return;
                }
            }
            // Try focus by learned command mapping.
            fn normalize_command_key(cmd: &str) -> Option<String> {
                let t = cmd.trim();
                if t.is_empty() { return None; }
                let tokens: Vec<&str> = t.split_whitespace().collect();
                if tokens.is_empty() { return None; }

                // flatpak run [options] <app_id> [...]
                if tokens.len() >= 2 && tokens[0] == "flatpak" && tokens[1] == "run" {
                    for tok in tokens.iter().skip(2) {
                        let s = *tok;
                        if !s.starts_with('-') && s.contains('.') {
                            return Some(s.to_string());
                        }
                    }
                    return tokens.last().map(|s| s.to_string());
                }

                // /usr/bin/flatpak run [options] <app_id> [...]
                if tokens.len() >= 3 && tokens[0] == "/usr/bin/flatpak" && tokens[1] == "run" {
                    for tok in tokens.iter().skip(2) {
                        let s = *tok;
                        if !s.starts_with('-') && s.contains('.') {
                            return Some(s.to_string());
                        }
                    }
                    return tokens.last().map(|s| s.to_string());
                }

                // gtk-launch <desktop_id>
                if tokens[0] == "gtk-launch" && tokens.len() >= 2 {
                    return Some(tokens[1].to_string());
                }

                // steam [-applaunch <id>]
                if tokens[0] == "steam" {
                    if let Some(idx) = tokens.iter().position(|&x| x == "-applaunch") {
                        if let Some(app) = tokens.get(idx + 1) {
                            return Some(format!("steam:{}", app));
                        }
                    }
                    return Some("steam".to_string());
                }

                // Fallback: first token
                Some(tokens[0].to_string())
            }
            let key = normalize_command_key(&command);
            if let Some(k) = key.as_ref() {
                if catacomb.windows.focus_mapped_command(k) {
                    catacomb.unstall();
                    return;
                }
            }
            catacomb.windows.note_launch_context(command.clone(), key, card_id, None);
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(&command);
            unsafe {
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
            match cmd.spawn() {
                Ok(mut child) => {
                    let pid = child.id() as i32;
                    catacomb.windows.update_pending_pgid(pid);
                    tracing::info!("IPC ExecOrFocus spawned: {}", command);
                    let _ = std::thread::spawn(move || {
                        let _ = child.wait();
                        let _ = std::process::Command::new("catacomb")
                            .arg("msg")
                            .arg("focus-role")
                            .arg("home")
                            .spawn();
                    });
                },
                Err(e) => tracing::error!("IPC ExecOrFocus failed: {} - {}", command, e),
            }
        },
        IpcMessage::FocusRole { role } => {
            // Try to focus by system role; fallback to default home matching.
            if role == "home" {
                // Try role mapping
                if let Some(matcher) = catacomb.windows.system_role("home") {
                    catacomb.focus_app(matcher);
                } else {
                    // Fallback to classic home ids
                    if let Ok(app) = AppIdMatcher::try_from("^(JollyPad-Desktop|jolly-home|JollyPad-Launcher)$".to_string()) {
                        catacomb.focus_app(app);
                    }
                }
            }
        },
        IpcMessage::ToggleWindow { app_id } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: toggle has invalid App ID regex: {err}");
                    return;
                },
            };

            if catacomb.windows.toggle_app(app_id) {
                catacomb.unstall();
            }
        },
        IpcMessage::SystemRole { role, app_id } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: system role has invalid App ID regex: {err}");
                    return;
                },
            };
            catacomb.windows.set_system_role(role, app_id);
        },
        IpcMessage::LogInput { state } => {
            let enabled = matches!(state, CliToggle::On);
            catacomb.input_log_enabled = enabled;
        },
        IpcMessage::DumpFocusGraph => {
            catacomb.windows.log_focus_graph();
        },
        IpcMessage::TraceScene { state } => {
            let enabled = matches!(state, CliToggle::On);
            catacomb.windows.set_trace_scene_enabled(enabled);
        },
        IpcMessage::RoleAction { role, action, payload } => {
            let role = role.as_str();
            let act = action.as_str();
            if (role == "nav" || role == "overlay") && act == "toggle" {
                if let Ok(app) = AppIdMatcher::try_from("JollyPad-Overlay".to_string()) {
                    if catacomb.windows.toggle_app(app) {
                        catacomb.unstall();
                    }
                }
                return;
            }
            if role == "overlay" && act == "back" {
                if let Ok(app) = AppIdMatcher::try_from("JollyPad-Overlay".to_string()) {
                    if catacomb.windows.toggle_app(app) {
                        catacomb.unstall();
                    }
                }
                return;
            }
            if role == "home" && act == "focus" {
                if let Some(matcher) = catacomb.windows.system_role("home") {
                    catacomb.focus_app(matcher);
                }
                return;
            }
            if role == "home" && act == "back" {
                // Minimal back: focus home if not already
                if let Some(matcher) = catacomb.windows.system_role("home") {
                    catacomb.focus_app(matcher);
                }
                return;
            }
            if role == "home" && act == "select" {
                catacomb.simulate_key(keysyms::KEY_Return, KeyState::Pressed);
                catacomb.simulate_key(keysyms::KEY_Return, KeyState::Released);
                return;
            }
            if role == "home" && act == "navigate" {
                match payload.as_deref() {
                    Some("up") => {
                        catacomb.simulate_key(keysyms::KEY_Up, KeyState::Pressed);
                        catacomb.simulate_key(keysyms::KEY_Up, KeyState::Released);
                    }
                    Some("down") => {
                        catacomb.simulate_key(keysyms::KEY_Down, KeyState::Pressed);
                        catacomb.simulate_key(keysyms::KEY_Down, KeyState::Released);
                    }
                    Some("left") => {
                        catacomb.simulate_key(keysyms::KEY_Left, KeyState::Pressed);
                        catacomb.simulate_key(keysyms::KEY_Left, KeyState::Released);
                    }
                    Some("right") => {
                        catacomb.simulate_key(keysyms::KEY_Right, KeyState::Pressed);
                        catacomb.simulate_key(keysyms::KEY_Right, KeyState::Released);
                    }
                    _ => {}
                }
                return;
            }
            if role == "nav" && act == "select" {
                // Placeholder: rely on UI to handle Return; no-op here
                return;
            }
            if role == "nav" && act == "navigate" {
                // Placeholder: payload could be up/down/left/right; currently no-op
                let _ = payload;
                return;
            }
        },
        IpcMessage::Exec { command } => {
            catacomb.windows.note_launch(command.clone());
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(&command);
            unsafe {
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
            match cmd.spawn() {
                Ok(mut child) => {
                    let pid = child.id() as i32;
                    catacomb.windows.update_pending_pgid(pid);
                    tracing::info!("IPC Exec spawned: {}", command);
                    let _ = std::thread::spawn(move || {
                        let _ = child.wait();
                        let _ = std::process::Command::new("catacomb")
                            .arg("msg")
                            .arg("focus-role")
                            .arg("home")
                            .spawn();
                    });
                },
                Err(e) => tracing::error!("IPC Exec failed: {} - {}", command, e),
            }
        },
        IpcMessage::GetActiveWindow => {
            let (title, app_id) = catacomb.active_window_info().unwrap_or_default();
            send_reply(&mut stream, &IpcMessage::ActiveWindow { title, app_id });
        },
        IpcMessage::GetClients => {
            let clients = catacomb.clients_info();
            send_reply(&mut stream, &IpcMessage::Clients { clients });
        },
        IpcMessage::Scale { scale, app_id: Some(app_id) } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: scale has invalid App ID regex: {err}");
                    return;
                },
            };

            catacomb.windows.add_window_scale(app_id, scale);
        },
        IpcMessage::Scale { scale, app_id: None } => {
            let scale = match scale {
                WindowScale::Fixed(scale) => scale,
                scale => {
                    warn!("ignoring invalid ipc message: expected fixed scale, got {scale:?}");
                    return;
                },
            };

            catacomb.windows.set_scale(scale);
            catacomb.unstall();
        },
        IpcMessage::BindGesture { app_id, start, end, program, arguments } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: binding has invalid App ID regex: {err}");
                    return;
                },
            };

            let action = GestureBindingAction::Cmd((program, arguments));
            let gesture = GestureBinding { app_id, start, end, action };
            catacomb.touch_state.user_gestures.push(gesture);
        },
        IpcMessage::BindGestureKey { app_id, start, end, mods, key } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: binding has invalid App ID regex: {err}");
                    return;
                },
            };

            // Ignore custom keysyms like virtual keyboard enable/disable.
            let key = match key {
                Keysym::Xkb(key) => key,
                _ => {
                    warn!("ignoring invalid ipc message: gestures must be bound to real keysyms");
                    return;
                },
            };

            let action = GestureBindingAction::Key((key, mods.unwrap_or_default()));
            let gesture = GestureBinding { app_id, start, end, action };
            catacomb.touch_state.user_gestures.push(gesture);
        },
        IpcMessage::UnbindGesture { app_id, start, end } => {
            catacomb.touch_state.user_gestures.retain(|gesture| {
                gesture.app_id.base() != app_id || gesture.start != start || gesture.end != end
            });
        },
        IpcMessage::BindKey { app_id, mods, trigger, keys, program, arguments } => {
            let app_id = match AppIdMatcher::try_from(app_id) {
                Ok(app_id) => app_id,
                Err(err) => {
                    warn!("ignoring invalid ipc message: binding has invalid App ID regex: {err}");
                    return;
                },
            };

            let binding = KeyBinding {
                arguments,
                program,
                trigger,
                app_id,
                keys,
                mods: mods.unwrap_or_default(),
            };
            catacomb.key_bindings.push(binding);
        },
        IpcMessage::UnbindKey { app_id, mods, keys } => {
            let mods = mods.unwrap_or_default();

            catacomb.key_bindings.retain(|binding| {
                binding.app_id.base() != app_id || binding.keys != keys || binding.mods != mods
            });
        },
        IpcMessage::KeyboardConfig { model, layout, variant, options } => {
            let model = model.as_deref().unwrap_or_default();
            let layout = layout.as_deref().unwrap_or_default();
            let variant = variant.as_deref().unwrap_or_default();
            let config = XkbConfig { model, layout, variant, options, rules: "" };
            catacomb.set_xkb_config(config);
        },
        IpcMessage::Dpms { state: Some(state) } => {
            catacomb.set_display_status(state == CliToggle::On)
        },
        IpcMessage::Dpms { state: None } => {
            let state = if catacomb.display_on { CliToggle::On } else { CliToggle::Off };
            send_reply(&mut stream, &IpcMessage::DpmsReply { state });
        },
        IpcMessage::Cursor { state } => {
            catacomb.draw_cursor = state == CliToggle::On;
        },
        IpcMessage::DumpWindows => {
            catacomb.windows.log_window_tree();
        },
        // Ignore IPC replies.
        IpcMessage::DpmsReply { .. } | IpcMessage::ActiveWindow { .. } | IpcMessage::Clients { .. } => (),
    }
}

/// Send IPC message reply.
fn send_reply(stream: &mut UnixStream, message: &IpcMessage) {
    if let Err(err) = send_reply_fallible(stream, message) {
        error!("Could not send IPC reply: {err}");
    }
}

/// Send IPC message reply, returning possible errors.
fn send_reply_fallible(
    stream: &mut UnixStream,
    message: &IpcMessage,
) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string(&message)?;
    stream.write_all(json.as_bytes())?;
    stream.flush()?;
    Ok(())
}
