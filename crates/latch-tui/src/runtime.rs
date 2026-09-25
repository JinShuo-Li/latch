//! Terminal lifecycle and the crossterm event loop.

use super::chrome::draw;
use super::*;

pub(super) struct Guard {
    pub(super) terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Guard {
    pub(super) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = enter_screen(&mut stdout) {
            leave_screen(&mut stdout).ok();
            disable_raw_mode().ok();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                disable_raw_mode().ok();
                let mut stdout = io::stdout();
                leave_screen(&mut stdout).ok();
                Err(error.into())
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        leave_screen(self.terminal.backend_mut()).ok();
        disable_raw_mode().ok();
        self.terminal.show_cursor().ok();
    }
}

pub(super) fn enter_screen(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
}

pub(super) fn leave_screen(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
}

pub async fn run(
    input_tx: mpsc::Sender<Input>,
    mut output_rx: mpsc::Receiver<Output>,
    mode: Mode,
    model: String,
    replay: Vec<DurableEvent>,
    history: Vec<String>,
) -> Result<()> {
    let mut guard = Guard::enter()?;
    let mut app = App {
        mode,
        model,
        branch: "-".into(),
        resumed: !replay.is_empty(),
        ..Default::default()
    };
    let session = SidebarSession {
        model: app.model.clone(),
        mode: app.mode,
        branch: app.branch.clone(),
        resumed: app.resumed,
        pricing: None,
    };
    for event in &replay {
        app.presentation.apply_event(event);
    }
    app.sidebar = SidebarModel::from_events(session, &replay);
    app.input.seed_history(history);
    let mut events = EventStream::new();
    loop {
        guard.terminal.draw(|frame| draw(frame, &mut app))?;
        tokio::select! {
         Some(out)=output_rx.recv()=>app.output(out),
         maybe=events.next()=>match maybe.transpose()?{
            Some(Event::Key(key)) if key.kind==KeyEventKind::Press => match app.on_key(key) {
                Some(Action::Submit { text, media }) => {
                    input_tx.send(Input::Submit { text, media }).await?
                }
                Some(Action::Attach(path)) => input_tx.send(Input::Attach(path)).await?,
                Some(Action::Cancel) => input_tx.send(Input::Cancel).await?,
                Some(Action::Resume) => { input_tx.send(Input::Resume).await?; break; }
                Some(Action::Quit) => { input_tx.send(Input::Quit).await?; break; }
                Some(Action::Permission { request_id, approved }) => {
                    input_tx.send(Input::Permission { request_id, approved }).await?;
                }
                Some(Action::SetSafety(safety)) => {
                    input_tx.send(Input::SetSafety(safety)).await?;
                }
                Some(Action::SetPermissions(mode)) => {
                    input_tx.send(Input::SetPermissions(mode)).await?;
                }
                Some(Action::SetInferenceProfile { provider, model, effort }) => {
                    input_tx
                        .send(Input::SetInferenceProfile {
                            provider,
                            model,
                            effort,
                        })
                        .await?;
                }
                Some(Action::SetupApply(plan)) => {
                    input_tx.send(Input::SetupApply(plan)).await?;
                }
                Some(Action::DiscoverModels { provider }) => {
                    input_tx.send(Input::DiscoverModels { provider }).await?;
                }
                None => {}
            },
            Some(Event::Mouse(mouse)) => {
                // The wheel scrolls whichever surface is under the pointer:
                // the composer when it overflows, otherwise the transcript.
                let over_composer = {
                    let r = app.composer_body;
                    r.width > 0
                        && mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                };
                let composer_scrollable = app
                    .input
                    .is_scrollable(app.last_input_width.max(1), app.last_input_height.max(1));
                match mouse.kind {
                    MouseEventKind::ScrollUp if over_composer && composer_scrollable => {
                        app.composer_scroll_up(WHEEL_ROWS)
                    }
                    MouseEventKind::ScrollDown if over_composer && composer_scrollable => {
                        app.composer_scroll_down(WHEEL_ROWS)
                    }
                    MouseEventKind::ScrollUp => app.scroll_up(WHEEL_ROWS),
                    MouseEventKind::ScrollDown => app.scroll_down(WHEEL_ROWS),
                    _ => {}
                }
            }
            Some(Event::Paste(text)) => app.on_paste(&text),
            None=>break,
            _=>{}
         }
        }
    }
    Ok(())
}
