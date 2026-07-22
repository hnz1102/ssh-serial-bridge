// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Hiroshi Nakajima
//
// Custom syslog implementation for ESP32-S3 platform

#![allow(dead_code)]

use log::{Log, Record, Level, Metadata, SetLoggerError, LevelFilter};
use std::sync::Mutex;
use std::net::UdpSocket;
use std::fmt::Write;
use std::time::SystemTime;
use chrono::{DateTime, Utc};
use std::io;

// Remote syslog server address
const SYSLOG_SERVER: &str = "192.168.2.140:514";

// Global logger instance protected by a mutex
static SYSLOGGER: Mutex<Option<SysLogger>> = Mutex::new(None);
// Static reference for the log system
static STATIC_LOGGER: StaticLoggerWrapper = StaticLoggerWrapper;

// Enumeration for syslog facilities (RFC 5424)
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum Facility {
    Kernel = 0,
    User = 1,
    Mail = 2,
    Daemon = 3,
    Auth = 4,
    Syslog = 5,
    Lpr = 6,
    News = 7,
    Uucp = 8,
    Cron = 9,
    AuthPriv = 10,
    Ftp = 11,
    Ntp = 12,
    LogAudit = 13,
    LogAlert = 14,
    Clock = 15,
    Local0 = 16,
    Local1 = 17,
    Local2 = 18,
    Local3 = 19,
    Local4 = 20,
    Local5 = 21,
    Local6 = 22,
    Local7 = 23,
}

// Enumeration for syslog severity levels (RFC 5424)
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum Severity {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Informational = 6,
    Debug = 7,
}

// Our custom logger that forwards logs to remote syslog server
pub struct SysLogger {
    socket: UdpSocket,
    level_filter: LevelFilter,
    server_addr: String,
    host_name: String,
    app_name: String,
}

impl SysLogger {
    // Format a log message according to RFC 5424 syslog protocol
    fn format_syslog_message(
        &self,
        facility: Facility,
        severity: Severity,
        timestamp: SystemTime,
        hostname: &str,
        app_name: &str,
        message: &str,
    ) -> String {
        let mut buffer = String::new();
        
        // PRI part: <PRI>
        // Convert enums to u8 using their numeric values directly
        let pri = ((facility as u8) << 3) | (severity as u8);
        let _ = write!(&mut buffer, "<{}>1 ", pri);
        
        // Format timestamp according to RFC 5424 (YYYY-MM-DDThh:mm:ss.sssZ)
        match DateTime::<Utc>::from(timestamp).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string().as_str() {
            formatted_time => {
                let _ = write!(&mut buffer, "{} ", formatted_time);
            }
        }
        
        // HOSTNAME
        let _ = write!(&mut buffer, "{} ", hostname);
        
        // APP-NAME
        let _ = write!(&mut buffer, "{} ", app_name);
        
        // PROCID and MSGID (using - as nil value)
        let _ = write!(&mut buffer, "- - ");
        
        // MSG
        let _ = write!(&mut buffer, "{}", message);
        
        buffer
    }

    fn send_message(&self, level: Severity, message: &str) {
        // Get current timestamp
        let timestamp = SystemTime::now();

        // Format the message according to RFC 5424
        let formatted_message = self.format_syslog_message(
            Facility::User,
            level,
            timestamp,
            &self.host_name,
            &self.app_name,
            message,
        );

        // Always echo to the local console too. Previously this logger only
        // wrote to the remote UDP syslog server, so once the server became
        // unreachable (Wi-Fi drop, wrong static IP, etc.) every log::info!/
        // warn!/error! call in the app became completely invisible on the
        // serial console — only a content-less "Failed to send..." line
        // remained, making it impossible to diagnose what was actually
        // happening (e.g. whether the WiFi monitor thread was retrying,
        // whether a reboot request had been received, etc.).
        println!("{}", message);

        // Send the message to the syslog server - using sendto instead of send
        // to avoid connection issues
        match self.socket.send_to(formatted_message.as_bytes(), &self.server_addr) {
            Ok(_) => {},
            Err(e) => {
                // Use eprintln for errors since we can't use the logger itself
                eprintln!("Failed to send log to syslog server: {}", e);
            }
        }
    }
}

impl Log for SysLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level_filter
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            // Map log levels to syslog severity
            let level = match record.level() {
                Level::Error => Severity::Error,
                Level::Warn => Severity::Warning,
                Level::Info => Severity::Informational,
                Level::Debug => Severity::Debug,
                Level::Trace => Severity::Debug,
            };
            
            // Format and send the message
            let message = format!("[{}] {}", record.target(), record.args());
            self.send_message(level, &message);
        }
    }

    fn flush(&self) {
        // UDP doesn't require explicit flushing
    }
}

// Static logger wrapper that lives for the entire program
pub struct StaticLoggerWrapper;

impl Log for StaticLoggerWrapper {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if let Ok(guard) = SYSLOGGER.lock() {
            if let Some(logger) = guard.as_ref() {
                return logger.enabled(metadata);
            }
        }
        false
    }
    
    fn log(&self, record: &Record) {
        if let Ok(guard) = SYSLOGGER.lock() {
            if let Some(logger) = guard.as_ref() {
                logger.log(record);
            }
        }
    }
    
    fn flush(&self) {
        if let Ok(guard) = SYSLOGGER.lock() {
            if let Some(logger) = guard.as_ref() {
                logger.flush();
            }
        }
    }
}

// Custom error type for logger initialization
#[derive(Debug)]
pub enum LoggerError {
    SocketError(io::Error),
    LockError,
    SetLoggerError(SetLoggerError),
}

impl From<io::Error> for LoggerError {
    fn from(error: io::Error) -> Self {
        LoggerError::SocketError(error)
    }
}

impl From<SetLoggerError> for LoggerError {
    fn from(error: SetLoggerError) -> Self {
        LoggerError::SetLoggerError(error)
    }
}

impl std::fmt::Display for LoggerError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            LoggerError::SocketError(e) => write!(f, "Socket error: {}", e),
            LoggerError::LockError => write!(f, "Failed to acquire lock on logger"),
            LoggerError::SetLoggerError(_) => write!(f, "Failed to set global logger"),
        }
    }
}

// Update logger configuration (hostname and app_name) without re-initializing the socket
pub fn update_logger_config(host_name: &str, app_name: &str) -> Result<(), LoggerError> {
    let mut guard = SYSLOGGER.lock().map_err(|_| {
        eprintln!("Failed to acquire lock on logger mutex");
        LoggerError::LockError
    })?;
    
    if let Some(logger) = guard.as_mut() {
        logger.host_name = host_name.to_string();
        logger.app_name = app_name.to_string();
        eprintln!("Syslog config updated: hostname={}, app_name={}", host_name, app_name);
    }
    
    Ok(())
}

// Initialize the syslogger with improved error handling
pub fn init_logger(syslog_server: &str, syslog_enable: &str, host_name: &str, app_name: &str) -> Result<(), LoggerError> {
    if syslog_enable != "true" {
        // syslog無効時は何もしない
        return Ok(());
    }
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| {
        eprintln!("Failed to create UDP socket for syslog: {}", e);
        LoggerError::SocketError(e)
    })?;
    socket.set_nonblocking(true).map_err(|e| {
        eprintln!("Failed to set socket to non-blocking mode: {}", e);
        LoggerError::SocketError(e)
    })?;
    if let Err(e) = socket.connect(syslog_server) {
        eprintln!("Warning: Failed to connect to syslog server {}: {}", syslog_server, e);
    }
    let sys_logger = SysLogger {
        socket,
        level_filter: LevelFilter::Info,
        server_addr: syslog_server.to_string(),
        host_name: host_name.to_string(),
        app_name: app_name.to_string(),
    };
    let test_message = format!("Syslog logger initialized for {}", app_name);
    sys_logger.send_message(Severity::Informational, &test_message);
    let mut guard = SYSLOGGER.lock().map_err(|_| {
        eprintln!("Failed to acquire lock on logger mutex");
        LoggerError::LockError
    })?;
    *guard = Some(sys_logger);
    drop(guard);
    log::set_logger(&STATIC_LOGGER)
        .map(|()| log::set_max_level(LevelFilter::Info))
        .map_err(|e| {
            eprintln!("Failed to set global logger: {:?}", e);
            LoggerError::SetLoggerError(e)
        })
}