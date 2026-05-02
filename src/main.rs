use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{self, Clear, ClearType},
};
use std::{
    cmp,
    env,
    fs,
    io::{self, stdout, Stdout, Write},
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const VERSION: &str = "0.2.3";
const SEARCH_STATUS_SECONDS: u64 = 5;
const MESSAGE_STATUS_SECONDS: u64 = 3;
const AI_STATUS_SECONDS: u64 = 9;
const POLL_FALLBACK_MS: u64 = 250;
const INDENT_WIDTH: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Language {
    PlainText,
    Python,
    C,
    Rust,
    Shell, // Added Shell variant
}

fn detect_language(filename: &str) -> Language {
    let ext = PathBuf::from(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "py" => Language::Python,
        "rs" => Language::Rust,
        "c" | "h" | "cpp" | "hpp" => Language::C,
        "sh" => Language::Shell, // Detect .sh files
        _ => Language::PlainText,
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        match args[1].as_str() {
            "--version" | "-v" => {
                println!(r#"__      __
\ \    / /  
 \ \  / /_ _ _ __  
  \ \/ / _` | '_ \ 
   \  / (_| | | | |
    \/ \__,_|_| |_|"#);

                println!("van editor version {}", VERSION);
                return Ok(());
            }
            "--help" | "-h" => {
                println!("van editor - a lightweight rust text editor");
                println!("\nUsage: van [FILENAME]");
                println!("\nControls:");
                println!("  Ctrl+S : Save");
                println!("  Ctrl+F : Find");
                println!("  Ctrl+Z : Undo");
                println!("  Ctrl+X : Exit");
                println!("  Esc    : Toggle command mode");
                println!("\nCommand mode:");
                println!("  :w      Save");
                println!("  :q      Quit if clean");
                println!("  :q!     Quit without saving");
                println!("  :wq     Save and quit");
                println!("  :wq!    Save and quit");
                println!("  :line   Jump to line");
                println!("  :chmod  Make .sh file executable");
                println!("  :!cmd   Run shell command");
                println!("  :ai ...  Ask Groq AI");
                return Ok(());
            }
            _ => {}
        }
    }

    let filename = if args.len() > 1 {
        args[1].clone()
    } else {
        "untitled.txt".to_string()
    };

    let mut out = stdout();
    let _guard = TerminalGuard::enter(&mut out)?;

    let mut editor = Editor::open(filename);
    editor.render(&mut out)?;

    loop {
        let timeout = editor.poll_timeout();
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    if editor.handle_key(key) {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    editor.request_full_redraw();
                }
                _ => {}
            }
        }

        if editor.tick() {
            editor.request_redraw();
        }

        if editor.needs_redraw() {
            editor.render(&mut out)?;
        }
    }

    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(out: &mut Stdout) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(out, terminal::EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Clone)]
enum UndoAction {
    InsertChar {
        y: usize,
        x: usize,
        ch: char,
    },
    DeleteChar {
        y: usize,
        x: usize,
        ch: char,
    },
    InsertNewline {
        y: usize,
        x: usize,
        right: String,
    },
    JoinLines {
        y: usize,
        x: usize,
        removed: String,
    },
}

#[derive(Clone)]
struct UndoEntry {
    action: UndoAction,
    cursor_x: usize,
    cursor_y: usize,
    offset_x: usize,
    offset_y: usize,
    dirty: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Insert,
    Command,
    AwaitGroqKey,
}

struct Editor {
    language: Language,
    filename: String,
    lines: Vec<String>,

    cursor_x: usize,
    cursor_y: usize,
    offset_x: usize,
    offset_y: usize,

    search_input: String,
    search_highlight: String,
    in_search: bool,

    confirm_exit: bool,

    mode: InputMode,
    command_buffer: String,

    groq_api_key: Option<String>,
    groq_key_buffer: String,
    pending_ai_request: Option<String>,

    temp_status: Option<(String, Instant)>,
    dirty: bool,

    undo_stack: Vec<UndoEntry>,

    needs_redraw: bool,
    force_full_redraw: bool,

    last_rendered_rows: Vec<String>,
    last_size: (u16, u16),

    ai_output: Option<Vec<String>>,
    ai_scroll: usize,
}

impl Editor {
    fn open(filename: String) -> Self {
        let language = detect_language(&filename);
        let lines = match fs::read_to_string(&filename) {
            Ok(text) => {
                let mut out: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                if out.is_empty() {
                    out.push(String::new());
                }
                out
            }
            Err(_) => vec![String::new()],
        };

        Self {
            language,
            filename,
            lines,
            cursor_x: 0,
            cursor_y: 0,
            offset_x: 0,
            offset_y: 0,
            search_input: String::new(),
            search_highlight: String::new(),
            in_search: false,
            confirm_exit: false,
            mode: InputMode::Insert,
            command_buffer: String::new(),
            groq_api_key: load_groq_api_key(),
            groq_key_buffer: String::new(),
            pending_ai_request: None,
            temp_status: None,
            dirty: false,
            undo_stack: Vec::new(),
            needs_redraw: true,
            force_full_redraw: true,
            last_rendered_rows: Vec::new(),
            last_size: (0, 0),

            ai_output: None,
            ai_scroll: 0,
        } 
    }

    fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    fn request_full_redraw(&mut self) {
        self.needs_redraw = true;
        self.force_full_redraw = true;
    }

    fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    fn poll_timeout(&self) -> Duration {
        if let Some((_, until)) = &self.temp_status {
            let now = Instant::now();
            if *until > now {
                return until
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(POLL_FALLBACK_MS));
            }
        }
        Duration::from_millis(POLL_FALLBACK_MS)
    }

    fn tick(&mut self) -> bool {
        if let Some((_, until)) = &self.temp_status {
            if Instant::now() >= *until {
                self.temp_status = None;
                return true;
            }
        }
        false
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.ai_output.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.ai_output = None;
                    self.request_full_redraw();
                }
                KeyCode::Up => {
                    if self.ai_scroll > 0 {
                        self.ai_scroll -= 1;
                        self.request_redraw();
                    }
                }
                KeyCode::Down => {
                    let max_scroll = self.ai_output
                        .as_ref()
                        .map(|lines| lines.len().saturating_sub(1))
                        .unwrap_or(0);

                    if self.ai_scroll < max_scroll {
                        self.ai_scroll += 1;
                        self.request_redraw();
                    }
                }
                _ => {}
            }
            return false;
        }

        if self.confirm_exit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return true,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.confirm_exit = false;
                    self.set_temp_status("Exit cancelled".to_string(), MESSAGE_STATUS_SECONDS);
                    self.request_redraw();
                }
                _ => {}
            }
            return false;
        }

        if self.in_search {
            match key.code {
                KeyCode::Enter => {
                    let query = self.search_input.clone();
                    self.in_search = false;

                    if query.is_empty() {
                        self.search_input.clear();
                        self.request_redraw();
                        return false;
                    }

                    self.search_highlight = query.clone();

                    if let Some((y, x)) = self.find_first(&query) {
                        self.cursor_y = y;
                        self.cursor_x = x;
                        self.set_temp_status(format!("Found '{}'", query), SEARCH_STATUS_SECONDS);
                    } else {
                        self.set_temp_status(format!("'{}' not found", query), SEARCH_STATUS_SECONDS);
                    }

                    self.request_full_redraw();
                }
                KeyCode::Esc => {
                    self.in_search = false;
                    self.search_input.clear();
                    self.set_temp_status("Find cancelled".to_string(), MESSAGE_STATUS_SECONDS);
                    self.request_redraw();
                }
                KeyCode::Backspace => {
                    self.search_input.pop();
                    self.request_redraw();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.search_input.push(c);
                    self.request_redraw();
                }
                _ => {}
            }
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('x') => {
                    self.confirm_exit = true;
                    self.request_redraw();
                }

                KeyCode::Char('s') => {
                    if self.save().is_ok() {
                        self.set_temp_status(format!("SAVED: {}", self.filename), MESSAGE_STATUS_SECONDS);
                    } else {
                        self.set_temp_status(format!("Save failed: {}", self.filename), MESSAGE_STATUS_SECONDS);
                    }
                    self.request_redraw();
                }

                KeyCode::Char('f') => {
                    self.in_search = true;
                    if self.search_highlight.is_empty() {
                        self.search_input.clear();
                    } else {
                        self.search_input = self.search_highlight.clone();
                    }
                    self.request_redraw();
                }

                KeyCode::Char('z') => {
                    if self.undo() {
                        self.set_temp_status("Undid last edit".to_string(), MESSAGE_STATUS_SECONDS);
                    } else {
                        self.set_temp_status("Nothing to undo".to_string(), MESSAGE_STATUS_SECONDS);
                    }
                    self.request_full_redraw();
                }

                _ => {}
            }
        }

        match self.mode {
            InputMode::AwaitGroqKey => {
                match key.code {
                    KeyCode::Esc => {
                        self.mode = InputMode::Insert;
                        self.groq_key_buffer.clear();
                        self.pending_ai_request = None;
                        self.set_temp_status("Groq key entry cancelled".to_string(), MESSAGE_STATUS_SECONDS);
                        self.request_redraw();
                    }
                    KeyCode::Enter => {
                        let key_value = self.groq_key_buffer.trim().to_string();
                        if key_value.is_empty() {
                            self.set_temp_status("Groq API key cannot be empty".to_string(), MESSAGE_STATUS_SECONDS);
                            self.request_redraw();
                            return false;
                        }

                        if save_groq_api_key(&key_value).is_ok() {
                            self.groq_api_key = Some(key_value);
                            self.mode = InputMode::Insert;
                            self.groq_key_buffer.clear();
                            self.set_temp_status("Groq API key saved".to_string(), MESSAGE_STATUS_SECONDS);
                            self.request_redraw();

                            if let Some(req) = self.pending_ai_request.take() {
                                self.run_ai_command(req);
                            }
                        } else {
                            self.set_temp_status("Failed to save Groq API key".to_string(), MESSAGE_STATUS_SECONDS);
                            self.request_redraw();
                        }
                    }
                    KeyCode::Backspace => {
                        self.groq_key_buffer.pop();
                        self.request_redraw();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.groq_key_buffer.push(c);
                        self.request_redraw();
                    }
                    _ => {}
                }
                return false;
            }

            InputMode::Command => {
                match key.code {
                    KeyCode::Esc => {
                        self.mode = InputMode::Insert;
                        self.command_buffer.clear();
                        self.set_temp_status("Command cancelled".to_string(), MESSAGE_STATUS_SECONDS);
                        self.request_redraw();
                    }
                    KeyCode::Enter => {
                        let command = std::mem::take(&mut self.command_buffer);
                        self.mode = InputMode::Insert;
                        self.request_redraw();
                        if self.execute_command(&command) {
                            return true;
                        }
                    }
                    KeyCode::Backspace => {
                        if self.command_buffer.len() > 1 {
                            self.command_buffer.pop();
                        }
                        self.request_redraw();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.command_buffer.push(c);
                        self.request_redraw();
                    }
                    _ => {}
                }
                return false;
            }

            InputMode::Insert => {}
        }

        match key.code {
            KeyCode::Esc => {
                self.mode = InputMode::Command;
                self.command_buffer.clear();
                self.command_buffer.push(':');
                self.set_temp_status("Command mode".to_string(), MESSAGE_STATUS_SECONDS);
                self.request_redraw();
            }

            KeyCode::Up => {
                if self.cursor_y > 0 {
                    self.cursor_y -= 1;
                    self.cursor_x = cmp::min(self.cursor_x, self.line_len(self.cursor_y));
                    self.request_redraw();
                }
            }

            KeyCode::Down => {
                if self.cursor_y + 1 < self.lines.len() {
                    self.cursor_y += 1;
                    self.cursor_x = cmp::min(self.cursor_x, self.line_len(self.cursor_y));
                    self.request_redraw();
                }
            }

            KeyCode::Left => {
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                } else if self.cursor_y > 0 {
                    self.cursor_y -= 1;
                    self.cursor_x = self.line_len(self.cursor_y);
                }
                self.request_redraw();
            }

            KeyCode::Right => {
                if self.cursor_x < self.line_len(self.cursor_y) {
                    self.cursor_x += 1;
                } else if self.cursor_y + 1 < self.lines.len() {
                    self.cursor_y += 1;
                    self.cursor_x = 0;
                }
                self.request_redraw();
            }

            KeyCode::Backspace => {
                self.backspace();
                self.request_full_redraw();
            }

            KeyCode::Enter => {
                self.insert_newline();
                self.request_full_redraw();
            }

            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(c);
                self.request_full_redraw();
            }

            _ => {}
        }

        false
    }

    fn execute_command(&mut self, command: &str) -> bool {
        let raw = command.trim();
        let raw = raw.strip_prefix(':').unwrap_or(raw).trim();

        if raw.is_empty() {
            self.set_temp_status("Empty command".to_string(), MESSAGE_STATUS_SECONDS);
            return false;
        }
        if let Ok(line_num) = raw.parse::<usize>() {
            if line_num > 0 && line_num <= self.lines.len() {
                self.cursor_y = line_num - 1; // 1-indexed to 0-indexed
                self.cursor_x = 0;             
                self.request_full_redraw();
                self.set_temp_status(format!("Jumped to line {}", line_num), MESSAGE_STATUS_SECONDS);
            } else {
                self.set_temp_status(
                    format!("Line {} is out of bounds (max: {})", line_num, self.lines.len()), 
                    MESSAGE_STATUS_SECONDS
                );
            }
            return false;
        }

        match raw {
            "w" => {
                if self.save().is_ok() {
                    self.set_temp_status(format!("SAVED: {}", self.filename), MESSAGE_STATUS_SECONDS);
                } else {
                    self.set_temp_status(format!("Save failed: {}", self.filename), MESSAGE_STATUS_SECONDS);
                }
                return false;
            }
            "q" => {
                if self.dirty {
                    self.set_temp_status(
                        "Unsaved changes. Use :q! to quit anyway.".to_string(),
                        MESSAGE_STATUS_SECONDS,
                    );
                    return false;
                }
                return true;
            }
            "q!" => {
                return true;
            }
            "wq" | "x" | "wq!" => {
                if self.save().is_ok() {
                    self.set_temp_status(format!("SAVED: {}", self.filename), MESSAGE_STATUS_SECONDS);
                    return true;
                } else {
                    self.set_temp_status(format!("Save failed: {}", self.filename), MESSAGE_STATUS_SECONDS);
                    return false;
                }
            }
            "chmod" => {
                // Feature: Only allow chmod on shell scripts detected via filename
                if self.language != Language::Shell {
                    self.set_temp_status("Error: :chmod only works for .sh files".to_string(), MESSAGE_STATUS_SECONDS);
                    return false;
                }

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    match fs::metadata(&self.filename) {
                        Ok(metadata) => {
                            let mut perms = metadata.permissions();
                            let mode = perms.mode();
                            // Add executable bit for user, group, and others (equivalent to chmod +x)
                            perms.set_mode(mode | 0o111); 
                            
                            if fs::set_permissions(&self.filename, perms).is_ok() {
                                self.set_temp_status("Permission worked: +x applied".to_string(), MESSAGE_STATUS_SECONDS);
                            } else {
                                self.set_temp_status("Permission failed: Failed to write permissions".to_string(), MESSAGE_STATUS_SECONDS);
                            }
                        }
                        Err(_) => {
                            self.set_temp_status("Permission failed: Save the file first!".to_string(), MESSAGE_STATUS_SECONDS);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    self.set_temp_status("Permission failed: chmod not supported on this OS".to_string(), MESSAGE_STATUS_SECONDS);
                }
                return false;
            }
            _ => {}
        }

        if let Some(shell_cmd) = raw.strip_prefix('!') {
            self.run_shell_command(shell_cmd.trim());
            return false;
        }

        if let Some(rest) = raw.strip_prefix("ai") {
            self.run_ai_command(rest.trim().to_string());
            return false;
        }

        self.set_temp_status(format!("Unknown command: :{}", raw), MESSAGE_STATUS_SECONDS);
        false
    }

    fn run_shell_command(&mut self, shell_cmd: &str) {
        if shell_cmd.trim().is_empty() {
            self.set_temp_status("Usage: :!<shell command>".to_string(), MESSAGE_STATUS_SECONDS);
            return;
        }

        let output = if cfg!(target_os = "windows") {
            Command::new("cmd").args(["/C", shell_cmd]).output()
        } else {
            Command::new("sh").arg("-c").arg(shell_cmd).output()
        };

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

                let msg = match (stdout.is_empty(), stderr.is_empty()) {
                    (true, true) => "[shell command produced no output]".to_string(),
                    (false, true) => stdout,
                    (true, false) => stderr,
                    (false, false) => format!("{} | {}", stdout, stderr),
                };

                self.set_temp_status(msg, MESSAGE_STATUS_SECONDS);
            }
            Err(e) => {
                self.set_temp_status(format!("Shell command failed: {}", e), MESSAGE_STATUS_SECONDS);
            }
        }
    }

    fn run_ai_command(&mut self, request: String) {
        let request = if request.trim().is_empty() {
            "Review this file and suggest fixes.".to_string()
        } else {
            request
        };

        if self.groq_api_key.is_none() {
            self.pending_ai_request = Some(request);
            self.mode = InputMode::AwaitGroqKey;
            self.groq_key_buffer.clear();
            self.set_temp_status("Enter Groq API key".to_string(), MESSAGE_STATUS_SECONDS);
            self.request_redraw();
            return;
        }

        let key = self.groq_api_key.as_ref().unwrap().clone();
        match self.call_groq_api(&key, &request) {
            Ok(reply) => {
                self.ai_output = Some(reply.lines().map(|l| l.to_string()).collect());
                self.ai_scroll = 0;
                self.request_full_redraw();
            }
            Err(e) => {
                self.set_temp_status(format!("Groq error: {}", e), AI_STATUS_SECONDS);
            }
        }
    }

    fn call_groq_api(&self, api_key: &str, request: &str) -> io::Result<String> {
        let file_text = self.lines.join("\n");
        let model = env::var("GROQ_MODEL").unwrap_or_else(|_| "llama-3.3-70b-versatile".to_string());

        let body = format!(
            r#"{{"model":"{}","messages":[{{"role":"system","content":"{}"}},{{"role":"user","content":"Current file:\n\n{}\n\nUser request:\n{}"}}],"temperature":0.2}}"#,
            json_escape(&model),
            json_escape("You are a concise coding assistant. Be practical and direct."),
            json_escape(&file_text),
            json_escape(request)
        );

        let output = Command::new("curl")
            .args([
                "-sS",
                "-X",
                "POST",
                "https://api.groq.com/openai/v1/chat/completions",
                "-H",
                &format!("Authorization: Bearer {}", api_key),
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
            ])
            .output()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("curl failed to start: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let msg = if !stderr.is_empty() { stderr } else { stdout };
            return Err(io::Error::new(
                io::ErrorKind::Other,
                if msg.is_empty() {
                    "Groq request failed".to_string()
                } else {
                    msg
                },
            ));
        }

        let response = String::from_utf8_lossy(&output.stdout).to_string();

        if let Some(reply) = extract_groq_content(&response) {
            if reply.trim().is_empty() {
                Err(io::Error::new(io::ErrorKind::Other, "empty Groq reply"))
            } else {
                Ok(reply)
            }
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "could not extract Groq response content",
            ))
        }
    }

    fn push_undo(&mut self, action: UndoAction) {
        self.undo_stack.push(UndoEntry {
            action,
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            offset_x: self.offset_x,
            offset_y: self.offset_y,
            dirty: self.dirty,
        });
    }

    fn undo(&mut self) -> bool {
        let Some(entry) = self.undo_stack.pop() else {
            return false;
        };

        match entry.action {
            UndoAction::InsertChar { y, x, .. } => {
                if let Some(line) = self.lines.get_mut(y) {
                    let byte_idx = char_to_byte_idx(line, x);
                    if byte_idx < line.len() {
                        line.remove(byte_idx);
                    }
                }
            }
            UndoAction::DeleteChar { y, x, ch } => {
                if let Some(line) = self.lines.get_mut(y) {
                    let byte_idx = char_to_byte_idx(line, x);
                    line.insert(byte_idx, ch);
                }
            }
            UndoAction::InsertNewline { y, x: _, right } => {
                if y + 1 < self.lines.len() {
                    let _ = self.lines.remove(y + 1);
                    if let Some(line) = self.lines.get_mut(y) {
                        line.push_str(&right);
                    }
                }
            }
            UndoAction::JoinLines { y, x, removed: _removed } => {
                if y > 0 && y - 1 < self.lines.len() {
                    let prev = &mut self.lines[y - 1];
                    let right = prev.split_off(x);
                    self.lines.insert(y, right);
                }
            }
        }

        self.cursor_x = entry.cursor_x;
        self.cursor_y = entry.cursor_y;
        self.offset_x = entry.offset_x;
        self.offset_y = entry.offset_y;
        self.dirty = entry.dirty;
        self.request_full_redraw();
        true
    }

    fn save(&mut self) -> io::Result<()> {
        let text = self.lines.join("\n");
        fs::write(&self.filename, text)?;
        self.dirty = false;
        Ok(())
    }

    fn set_temp_status(&mut self, msg: String, seconds: u64) {
        self.temp_status = Some((msg, Instant::now() + Duration::from_secs(seconds)));
    }

    fn current_status(&self) -> String {
        if self.confirm_exit {
            return "Exit without saving? (y = quit, n = cancel)".to_string();
        }

        if self.in_search {
            return format!("Search: {}", self.search_input);
        }

        if self.mode == InputMode::Command {
            return format!("Command: {}", self.command_buffer);
        }

        if self.mode == InputMode::AwaitGroqKey {
            let masked = "*".repeat(self.groq_key_buffer.chars().count());
            return format!(
                "Groq API key: {} | Enter = save | Esc = cancel",
                masked
            );
        }

        if let Some((msg, until)) = &self.temp_status {
            if Instant::now() < *until {
                return msg.clone();
            }
        }

        let dirty = if self.dirty { "*" } else { "" };
        format!(
            "{}{} | Ctrl+S Save | Ctrl+F Find | Ctrl+Z Undo | Ctrl+X Exit | Esc Command",
            dirty, self.filename
        )
    }

    fn update_scroll(&mut self, width: usize, height: usize) {
        let text_rows = height.saturating_sub(1);

        if self.cursor_y < self.offset_y {
            self.offset_y = self.cursor_y;
        } else if self.cursor_y >= self.offset_y + text_rows {
            self.offset_y = self.cursor_y.saturating_sub(text_rows.saturating_sub(1));
        }

        if self.cursor_x < self.offset_x {
            self.offset_x = self.cursor_x;
        } else if self.cursor_x >= self.offset_x + width {
            self.offset_x = self.cursor_x.saturating_sub(width.saturating_sub(1));
        }
    }

    fn render(&mut self, out: &mut Stdout) -> io::Result<()> {
        let (w_u16, h_u16) = terminal::size()?;
        let width = w_u16 as usize;
        let height = h_u16 as usize;

        if let Some(ai_lines) = &self.ai_output {
            queue!(out, Clear(ClearType::All))?;

            let text_rows = height.saturating_sub(1);

            for i in 0..text_rows {
                let idx = self.ai_scroll + i;
                if idx < ai_lines.len() {
                    let line = truncate_to_width(&ai_lines[idx], width);
                    queue!(out, cursor::MoveTo(0, i as u16), Print(line))?;
                }
            }

            let status = "[AI VIEW] ↑/↓ scroll | Esc to exit";
            let padded = pad_to_width(&truncate_to_width(status, width), width);

            if height > 0 {
                queue!(
                    out,
                    cursor::MoveTo(0, (height - 1) as u16),
                    SetAttribute(Attribute::Reverse),
                    Print(padded),
                    SetAttribute(Attribute::Reset)
                )?;
            }

            out.flush()?;
            self.needs_redraw = false;
            self.force_full_redraw = false;
            return Ok(());
        }

        self.update_scroll(width, height);

        let text_rows = height.saturating_sub(1);
        let status_row = height.saturating_sub(1);

        let current_rows = self.build_rows(width, text_rows);

        let size_changed = self.last_size != (w_u16, h_u16);
        let full_redraw = self.force_full_redraw
            || size_changed
            || self.last_rendered_rows.len() != current_rows.len();

        for row in 0..text_rows {
            let new_text = current_rows.get(row).map(String::as_str).unwrap_or("");
            let old_text = self.last_rendered_rows.get(row).map(String::as_str).unwrap_or("");

            if full_redraw || new_text != old_text {
                queue!(
                    out,
                    cursor::MoveTo(0, row as u16),
                    Clear(ClearType::CurrentLine)
                )?;
                self.draw_visible_line(out, row, width)?;
            }
        }

        let status = self.current_status();
        let padded_status = pad_to_width(&truncate_to_width(&status, width), width);
        let old_status = self
            .last_rendered_rows
            .get(text_rows)
            .map(String::as_str)
            .unwrap_or("");

        if full_redraw || padded_status != old_status {
            queue!(
                out,
                cursor::MoveTo(0, status_row as u16),
                SetAttribute(Attribute::Reverse),
                Clear(ClearType::CurrentLine),
                Print(&padded_status),
                SetAttribute(Attribute::Reset)
            )?;
        }

        if height > 0 {
            let cx = self
                .cursor_x
                .saturating_sub(self.offset_x)
                .min(width.saturating_sub(1)) as u16;
            let cy = self
                .cursor_y
                .saturating_sub(self.offset_y)
                .min(text_rows.saturating_sub(1)) as u16;
            queue!(out, cursor::MoveTo(cx, cy))?;
        }

        out.flush()?;

        self.last_rendered_rows = current_rows;
        if self.last_rendered_rows.len() == text_rows {
            self.last_rendered_rows.push(padded_status);
        } else {
            if self.last_rendered_rows.len() > text_rows {
                self.last_rendered_rows.truncate(text_rows);
            }
            self.last_rendered_rows.push(padded_status);
        }

        self.last_size = (w_u16, h_u16);
        self.force_full_redraw = false;
        self.needs_redraw = false;

        Ok(())
    }

    fn build_rows(&self, width: usize, text_rows: usize) -> Vec<String> {
        let mut rows = Vec::with_capacity(text_rows + 1);

        for i in 0..text_rows {
            let line_idx = self.offset_y + i;
            if line_idx < self.lines.len() {
                rows.push(self.visible_plain_text(&self.lines[line_idx], width));
            } else {
                rows.push(String::new());
            }
        }

        rows.push(self.current_status());
        rows
    }

    fn draw_visible_line(&self, out: &mut Stdout, row: usize, width: usize) -> io::Result<()> {
        let line_idx = self.offset_y + row;
        if line_idx >= self.lines.len() {
            return Ok(());
        }

        let line = &self.lines[line_idx];
        let start_byte = char_to_byte_idx(line, self.offset_x);
        let end_byte = char_to_byte_idx(line, self.offset_x + width);
        let visible = &line[start_byte..end_byte];

        if self.search_highlight.is_empty() {
            queue!(out, Print(sanitize_str(visible)))?;
            return Ok(());
        }

        let query = self.search_highlight.as_str();
        let mut idx = 0;

        while idx < visible.len() {
            if let Some(pos) = visible[idx..].find(query) {
                let abs = idx + pos;
    
                if abs > idx {
                    queue!(out, Print(sanitize_str(&visible[idx..abs])))?;
                }

                let end = abs + query.len();
                queue!(
                    out,
                    SetAttribute(Attribute::Reverse),
                    Print(sanitize_str(&visible[abs..end])),
                    SetAttribute(Attribute::Reset)
                )?;

                idx = end;
            } else {
                queue!(out, Print(sanitize_str(&visible[idx..])))?;
                break;
            }
        }

        Ok(())
    }

    fn visible_plain_text(&self, line: &str, width: usize) -> String {
        let start_byte = char_to_byte_idx(line, self.offset_x);
        let end_byte = char_to_byte_idx(line, self.offset_x + width);
        sanitize_str(&line[start_byte..end_byte])
    }

    fn line_len(&self, y: usize) -> usize {
        self.lines[y].chars().count()
    }

    fn insert_char(&mut self, c: char) {
        let y = self.cursor_y;
        let x = self.cursor_x;
        self.push_undo(UndoAction::InsertChar { y, x, ch: c });

        let line = &mut self.lines[y];
        let byte_idx = char_to_byte_idx(line, x);
        line.insert(byte_idx, c);
        self.cursor_x += 1;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor_x > 0 {
            let y = self.cursor_y;
            let x = self.cursor_x - 1;
            if let Some(ch) = self.lines[y].chars().nth(x) {
                self.push_undo(UndoAction::DeleteChar { y, x, ch });

                let line = &mut self.lines[y];
                let byte_idx = char_to_byte_idx(line, x);
                line.remove(byte_idx);
                self.cursor_x -= 1;
                self.dirty = true;
            }
        } else if self.cursor_y > 0 {
            let y = self.cursor_y;
            let x = self.line_len(y - 1);
            let removed = self.lines[y].clone();
            self.push_undo(UndoAction::JoinLines { y, x, removed });

            let current = self.lines.remove(y);
            self.cursor_y -= 1;
            let prev_len = self.line_len(self.cursor_y);
            self.lines[self.cursor_y].push_str(&current);
            self.cursor_x = prev_len;
            self.dirty = true;
        }
    }
    fn leading_indent(line: &str) -> usize {
        line.chars().take_while(|c| *c == ' ').count()
    }

    fn compute_indent(&self, left: &str, right: &str) -> usize {
        let base = Self::leading_indent(left);
        let left_trim = left.trim_end();
        let right_trim = right.trim_start();

        match self.language {
            Language::Python => {
                let mut indent = base;

                if left_trim.ends_with(':') {
                indent += INDENT_WIDTH;
                }

                if right_trim.starts_with("elif ")
                    || right_trim.starts_with("else:")
                    || right_trim.starts_with("except")
                {
                    indent = indent.saturating_sub(INDENT_WIDTH);
                }

                indent
            }

            Language::C | Language::Rust => {
                let mut indent = base;

                if left_trim.ends_with('{') {
                    indent += INDENT_WIDTH;
                }

                if right_trim.starts_with('}') {
                    indent = indent.saturating_sub(INDENT_WIDTH);
                }

                indent
            }

            Language::PlainText | Language::Shell => base,
        }
    }
    fn insert_newline(&mut self) {
        let y = self.cursor_y;
        let x = self.cursor_x;

        let split_byte = {
            let line = &self.lines[y];
            char_to_byte_idx(line, x)
        };

        let left = self.lines[y][..split_byte].to_string();
        let right = self.lines[y][split_byte..].to_string();

        let indent = self.compute_indent(&left, &right);

        self.push_undo(UndoAction::InsertNewline {
            y,
            x,
            right: right.clone(),
        });

        self.lines[y] = left;

        let mut new_line = " ".repeat(indent);
        new_line.push_str(&right);

        self.lines.insert(y + 1, new_line);
        self.cursor_y += 1;
        self.cursor_x = indent;
        self.dirty = true;
    }

    fn find_first(&self, query: &str) -> Option<(usize, usize)> {
        for (y, line) in self.lines.iter().enumerate() {
            if let Some(byte_idx) = line.find(query) {
                let char_idx = line[..byte_idx].chars().count();
                return Some((y, char_idx));
            }
        }
        None
    }
}

fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }

    match s.char_indices().nth(char_idx) {
        Some((byte_idx, _)) => byte_idx,
        None => s.len(),
    }
}

fn truncate_to_width(s: &str, width: usize) -> String {
    s.chars().take(width).collect()
}

fn pad_to_width(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn extract_groq_content(resp: &str) -> Option<String> {
    let marker = "\"content\":\"";
    let start = resp.find(marker)? + marker.len();
    let mut out = String::new();
    let mut chars = resp[start..].chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => {
                let esc = chars.next()?;
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{08}'),
                    'f' => out.push('\u{0C}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let mut hex = String::new();
                        for _ in 0..4 {
                            hex.push(chars.next()?);
                        }
                        if let Ok(code) = u16::from_str_radix(&hex, 16) {
                            if let Some(c) = char::from_u32(code as u32) {
                                out.push(c);
                            }
                        }
                    }
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
    }

    None
}

fn groq_api_key_path() -> Option<PathBuf> {
    let base = if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = env::var_os("HOME") {
        PathBuf::from(home).join(".config")
    } else if let Some(profile) = env::var_os("USERPROFILE") {
        PathBuf::from(profile)
    } else {
        return None;
    };

    Some(base.join("van_groq_api_key"))
}

fn load_groq_api_key() -> Option<String> {
    let path = groq_api_key_path()?;
    fs::read_to_string(path).ok().and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn save_groq_api_key(key: &str) -> io::Result<()> {
    let path = groq_api_key_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config path available"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&path, key.trim())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

fn sanitize_str(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c == '\x1b' {
                Some('␛')
            } else if c.is_control() && !['\t', '\n', '\r'].contains(&c) {
                None
            } else {
                Some(c)
            }
        })
        .collect()
}