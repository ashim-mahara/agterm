use ptyprocess::PtyProcess;
use std::fs::File;
// use std::ops::BitXor;
use std::process::Command;
use std::io::{BufReader, BufWriter, Read, Write};
use std::thread::JoinHandle;
use pyo3::prelude::*;
use std::{str};
use pyo3::types::PyModule;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::{thread, time, sync::Mutex};

#[pyclass]
struct AGTerm {
    process: PtyProcess,
    // buffer_write_channel: Sender<String>,
    buffer_read_channel: Mutex<Receiver<String>>,
    // buffer_writer: Mutex<BufWriter<File>>,
    command: String,
    // initial_state: Mutex<String>,
    interactive: bool,
    history: Mutex<String>,
    read_offset: Mutex<usize>,
    // read_handler: JoinHandle<()>
}


#[pymethods]
impl AGTerm {
    #[new]
    fn new(command: String, interactive: bool) -> PyResult<Self> {
        let os_command = Command::new(command.clone());
        let process = match PtyProcess::spawn(os_command) {
        // let process = match PtyProcess::spawn(Command::new("/tmp/test")) {

            Ok(process) => process,
            Err(e) => {
                println!("Failed to spawn a process with command: {}, Error: {}", command, e);
                return Err(PyErr::new::<pyo3::exceptions::PyException, _>(format!("Failed to spawn a process with command: {}", command)));
            }
        };

        let stream = process.get_raw_handle().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let _buffer_writer_: Mutex<BufWriter<File>> = Mutex::new(BufWriter::new(stream));
        let (sender_channel, receiver_channel): (Sender<String>, Receiver<String>) = channel();

        let _read_handler_ = spawn_reader_channel::<File>(reader, &sender_channel.clone(), 10); // deal with later

        // let buffer_write_channel = sender_channel.clone();
        let buffer_read_channel = Mutex::new(receiver_channel);

        Ok(AGTerm {
            process,
            // buffer_write_channel,
            buffer_read_channel,
            // buffer_writer,
            command,
            // initial_state: Mutex::new("".to_string()),
            interactive,
            history: Mutex::new(String::new()),
            read_offset: Mutex::new(0),
            // read_handler
        })
    }

    fn write_to_stream(&mut self, command: &str) -> Result<(), std::io::Error> {
        let stream = self.process.get_raw_handle().unwrap();

        let buffer_writer = Mutex::new(BufWriter::new(stream));

        let mut full_command = command.to_string();
        full_command += "\n";

        buffer_writer.lock().unwrap().write_all(full_command.as_bytes()).map_err(|e| {
            println!("Failed to write to the stream: {}", e);
            e
        })?;
        buffer_writer.lock().unwrap().flush().expect("failed to flush the stream");
        Ok(())
    }

    pub fn get_history(&self) -> PyResult<String> {
        Ok(self.history.lock().unwrap().clone())
    }

    pub fn reset(&mut self) {
        self.close().unwrap();
        *self = AGTerm::new(self.command.clone(), self.interactive).unwrap();
        let initial_state = self.read_from_stream().unwrap();
        println!("Initial State: {}", initial_state);
    }

    pub fn is_alive(&self) -> PyResult<bool> {
        Ok(self.process.is_alive().expect("failed to check if the process is alive"))
    }

    pub fn is_interactive(&self) -> PyResult<bool> {
        Ok(self.interactive)
    }

    pub fn get_initial_command(&self) -> PyResult<String> {
        Ok(self.command.clone())
    }

    pub fn close(&mut self) -> PyResult<()> {
        assert!(self.process.exit(true).expect("failed to stop the process"));
        Ok(())
    }

    fn read_from_stream(&mut self) -> PyResult<String> {
        let mut buf: Vec<u8> = Vec::new();
        let timeout = 100;

        println!("Reading from the stream");

        // buf.extend_from_slice(self.initial_state.lock().unwrap().as_bytes());
        let mut consecutive_empty_lines = 0;
        let max_consecutive_empty_lines = 3;

        for __ in 0..timeout {
            // print!(".");
            match self.buffer_read_channel.lock().unwrap().try_recv() {
                Ok(line) => {
                    // println!("<CHANNEL_READ_STRING> {} <CHANNEL_READ_STRING>", line);
                    // thread::sleep(time::Duration::from_millis(1));
                    buf.extend_from_slice(line.as_bytes());
                    if line != "\n" {
                        consecutive_empty_lines = 0;
                    };
                }
                // Err(e) => {
                //     println!("Error while reading: {}", e);
                //     break;
                // }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // println!("Empty line");
                    if consecutive_empty_lines >= 0 && self.interactive {
                        thread::sleep(time::Duration::from_secs(2));
                        // thread::sleep(time::Duration::from_millis(5));
                        // continue;
                    }
                    consecutive_empty_lines += 1;
                    if consecutive_empty_lines >= max_consecutive_empty_lines {
                        // println!("Breaking cause of consecutive empty lines: 137");
                        break;
                    }
                    // break;
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    println!("Error: Channel disconnected");
                    break;
                }
            }
        }

        let s = match str::from_utf8(&buf) {
            Ok(v) => v.to_string(),
            Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
        };

        // print!("<String_READ_BEGIN> {} <String_READ_END>", s);

        // Append new data to history and determine the new portion to return
        let mut history = self.history.lock().unwrap();
        let mut read_offset = self.read_offset.lock().unwrap();

        let previous_offset = *read_offset;
        history.push_str(&s);

        let new_data = history[previous_offset..].to_string();
        *read_offset = history.len();

        Ok(new_data)
    }

    pub fn execute_command(&mut self, command: &str) -> PyResult<String> {

        match self.write_to_stream(command) {
            Ok(_) => {
                // println!("Successfully wrote to the stream");
            },
            Err(e) => println!("Failed to write to the stream: {}", e),
        };

        let res = self.read_from_stream().unwrap();
        // res = self.initial_state.lock().unwrap().clone() + &res;
        // println!("\nRESULT_START {} RESULT_END", res);
        Ok(res)
    }
}

//reads from reader and writes to the sender channel
fn spawn_reader_channel<T>(mut reader: BufReader<File>, sender_channel: &Sender<String>, _timeout: i32) -> JoinHandle<()> {
    let tx = sender_channel.clone();
    // let mut len_buf: Vec<u8> = Vec::new();

    let handler = thread::Builder::new().name("blocking_reader_thread".to_string()).spawn(move || loop {
        thread::sleep(time::Duration::from_millis(5));
        let mut buffer: Vec<u8> = vec![0; 8];
        match reader.read(&mut buffer) {
            Ok(num_bytes) => {
                if num_bytes == 0 {
                    println!("breaking cause 0 bytes");
                    break;
                }
                // &buffer[..num_bytes].to_ascii_lowercase();

                // println!("xor res: {:?}", &buffer[..num_bytes].cmp(&31));
                // println!("escape char bool: {:?}", &buffer[..num_bytes].xor_bit(&[0b00011111u8]));

                // println!("<READER_READ_NUM_BYTES> {} <READER_READ_NUM_BYTES>", num_bytes);
                // let byte_str = b"00011111";
                // assert_eq!(byte_str, &*byte_vec);

                // print!("<READER_READ_RAW_BYTES> {:?} <READER_READ_RAW_BYTES>", &buffer[..num_bytes].escape_ascii().to_string());
                // print!("<READER_READ_BYTES_TO_STRING> {:?} <READER_READ_BYTES_TO_STRING>", &buffer[..num_bytes]);

                // let line = String::from_utf8_lossy(&buffer[..num_bytes].escape_ascii().to_string()).to_string();
                // let line = String::from_utf8();
                let line = buffer[..num_bytes].escape_ascii().to_string();
                if line.is_empty() {
                    // println!("breaking cause empty line");
                    continue;
                }
                // println!("<READER_READ_STRING> {} <READER_READ_STRING>", line);
                tx.send(line).unwrap();
                thread::sleep(time::Duration::from_millis(1));
            }
            Err(e) => {
                let raw_error = e.raw_os_error().unwrap();
                if raw_error == 5 {
                    // this is the last thing to exit right now
                    print!("Process Exited");
                    // println!("sleep indefinitely because of error 5");
                    // thread::sleep(time::Duration::from_secs(4000));
                    // println!("Reached EOF");
                    // continue;
                    // println!("Reached EOF");
                    break;
                } else {
                    println!("Error while reading: {}, Raw Error: {}", e, raw_error);
                    break;
                }
            }
        }
    }).unwrap();
    handler
}


pub fn read_from_channel(read_channel: Receiver<String>, timeout: i32) -> Result<String, Box<dyn std::error::Error>> {
    let mut buf: Vec<u8> = Vec::new();
    let timeout = 100;

    // println!("Reading from the stream");

    // buf.extend_from_slice(self.initial_state.as_bytes());
    let mut consecutive_empty_lines = 0;
    let max_consecutive_empty_lines = 100;
    for __ in 0..timeout {
        // print!(".");
        match read_channel.try_recv() {
            Ok(line) => {
                buf.extend_from_slice(line.as_bytes());
                consecutive_empty_lines = 0;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if consecutive_empty_lines >= 1 {
                    thread::sleep(time::Duration::from_secs(1));
                    // continue;
                }
                consecutive_empty_lines += 1;
                if consecutive_empty_lines >= max_consecutive_empty_lines {
                    // println!("Breaking cause of consecutive empty lines");
                    break;
                }
                // break;
                continue;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                println!("Error: Channel disconnected");
                break;
            }
        }
    }

    let s = match str::from_utf8(&buf) {
        Ok(v) => v.to_string(),
        Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
    };
    Ok(s)
    // Ok(new_data)
}

/// A Python module implemented in Rust.
#[pymodule(name="agterm", gil_used = false)]
fn agterm(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AGTerm>()?;
    Ok(())
}