use tracing::{Event, Subscriber};
use tracing_subscriber::{
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter,
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use std::path::PathBuf;

struct RedactingFormatter;

impl<S, N> FormatEvent<S, N> for RedactingFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        write!(writer, "{} {} {}: ", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), meta.level(), meta.target())?;
        
        let mut visitor = RedactingVisitor { writer: &mut writer };
        event.record(&mut visitor);
        
        writeln!(writer)
    }
}

struct RedactingVisitor<'a, 'writer> {
    writer: &'a mut Writer<'writer>,
}

impl<'a, 'writer> tracing::field::Visit for RedactingVisitor<'a, 'writer> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let name = field.name().to_lowercase();
        if name.contains("token") || name.contains("secret") || name.contains("authorization") || name.contains("client_id") {
            let _ = write!(self.writer, "{} = [REDACTED] ", field.name());
        } else {
            let _ = write!(self.writer, "{} = {:?} ", field.name(), value);
        }
    }
}

pub fn init_logging(level: &str) {
    let mut log_dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    log_dir.push("clouddrive");
    log_dir.push("logs");
    
    std::fs::create_dir_all(&log_dir).unwrap_or_default();
    
    let file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "clouddrive.log");
    
    let env_filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    
    let fmt_layer = tracing_subscriber::fmt::layer()
        .event_format(RedactingFormatter)
        .with_writer(file_appender);
        
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init();
}
