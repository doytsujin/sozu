use network::buffer::Buffer;
use pool::Reset;
use std::io::{self,Write};
use nom::HexDisplay;
use std::cmp::max;

#[derive(Debug,PartialEq,Clone)]
pub enum InputElement {
  /// length in the stream
  Slice(usize),
  Splice(usize), // x bytes copied in kernel
}

#[derive(Debug,PartialEq,Clone)]
pub enum OutputElement {
  /// length in the stream
  Slice(usize),
  Delete(usize),
  Insert(Vec<u8>),
  Splice(usize), // should copy x bytes from kernel to socket
}

/// The BufferQueue has two roles: holding incoming data, and indicating
/// which data will go out. When new data arrives, it is added at the
/// end of the internal buffer. This new data is then eventually parsed or
/// handled in some way by external code. The external code then adds
/// element to the queue, indicating what to do with the data:
///   - copy a subset of the input data (and advance if needed)
///   - insert external data, like a HTTP header
///   - splice out of the kernel some data that was spliced in
///
/// position is the index in the stream of data already handled.
/// it corresponds to the beginning of available data in the Buffer
/// a Slice(begin, end) would point to buffer.data()[begin-position..end-position]
/// (in the easiest case)
///
/// unparsed_position is the index in the stream of data that was
/// not parsed yet
///
/// The buffer's available data may be smaller than `end - begin`.
/// It can happen if the parser indicated we need to copy more data than is available,
/// like with a content length
///
/// should the buffer queue indicate how much data it needs?
//#[derive(Debug,PartialEq,Clone)]
pub struct BufferQueue {
  /// position of buffer start in stream
  pub buffer_position:        usize,
  pub parsed_position:        usize,
  pub output_position:        usize,
  pub start_parsing_position: usize,
  pub buffer:                 Buffer,
  /// Vec<(start, length)>
  pub input_queue:            Vec<InputElement>,
  pub output_queue:           Vec<OutputElement>,
}

impl BufferQueue {
  pub fn with_capacity(capacity: usize) -> BufferQueue {
    BufferQueue {
      buffer_position:        0,
      parsed_position:        0,
      output_position:        0,
      start_parsing_position: 0,
      buffer:                 Buffer::with_capacity(capacity),
      input_queue:            Vec::with_capacity(8),
      output_queue:           Vec::with_capacity(8),
    }
  }

  pub fn available_input_data(&self) -> usize {
    self.input_queue.iter().fold(0, |acc, el| {
      acc + match el {
        &InputElement::Slice(sz) | &InputElement::Splice(sz) => sz
      }
    })
  }

  pub fn spliced_input(&mut self, count: usize) {
    self.input_queue.push(InputElement::Splice(count));
  }

  pub fn merge_input_slices(&self) -> usize {
    let mut acc = 0usize;
    for el in self.input_queue.iter() {
      match el {
        &InputElement::Splice(_) => break,
        &InputElement::Slice(sz) => acc += sz,
      }
    }

    assert!(acc <= self.buffer.available_data(), "the merged input slices can't be larger than current data in buffer");
    acc
  }

  pub fn input_data_size(&self) -> usize {
    let mut acc = 0usize;
    for el in self.input_queue.iter() {
      match el {
        &InputElement::Splice(sz) => acc += sz,
        &InputElement::Slice(sz)  => acc += sz,
      }
    }
    acc
  }

  pub fn unparsed_data(&self) -> &[u8] {
    let largest_size = self.merge_input_slices();
    if largest_size == 0 {
      return &self.buffer.data()[0..0];
    }
    let end = max(self.buffer.data().len(), self.parsed_position+largest_size);
    &self.buffer.data()[self.parsed_position..end]
  }

  /// should only be called with a count inferior to self.input_data_size()
  pub fn consume_parsed_data(&mut self, size: usize) {
    //FIXME: to_consume must contain unparsed_position - parsed_position ?
    let mut to_consume = size;
    while to_consume > 0 {
      let new_first_element = match self.input_queue.first() {
        None => {
          //assert!(to_consume == 0, "no more element in queue, we should not ask to consume {} more bytes", to_consume);
          break;
        },
        Some(&InputElement::Slice(sz)) => {
          if to_consume >= sz {
            to_consume -= sz;
            None
          } else {
            let new_element = InputElement::Slice(sz - to_consume);
            to_consume = 0;
            Some(new_element)
          }
        },
        Some(&InputElement::Splice(sz)) => {
          if to_consume >= sz {
            to_consume -= sz;
            None
          } else {
            panic!("we should not start parsing from inside a splicing buffer. But what if consume_parsed_data was called during a parsing loop? Should only call consume_parsed_data after the parsing loop finished");
          }
        },
      };

      match new_first_element {
        None     => { self.input_queue.remove(0); },
        Some(el) => { self.input_queue[0] = el; },
      };
    }

    self.parsed_position        += size - to_consume;
    self.start_parsing_position += size;
  }


  pub fn slice_output(&mut self, count: usize) {
    self.output_queue.push(OutputElement::Slice(count));
  }

  pub fn delete_output(&mut self, count: usize) {
    self.output_queue.push(OutputElement::Delete(count));
  }

  pub fn splice_output(&mut self, count: usize) {
    self.output_queue.push(OutputElement::Splice(count));
  }

  pub fn insert_output(&mut self, v: Vec<u8>) {
    self.output_queue.push(OutputElement::Insert(v));
  }

  pub fn output_data_size(&self) -> usize {
    let mut acc = 0usize;
    for el in self.output_queue.iter() {
      match el {
        &OutputElement::Splice(sz)    => acc += sz,
        &OutputElement::Slice(sz)     => acc += sz,
        &OutputElement::Insert(ref v) => acc += v.len(),
        &OutputElement::Delete(_)     => {},
      }
    }
    acc
  }

  pub fn merge_output_slices(&self) -> usize {
    let mut acc = 0usize;
    for el in self.output_queue.iter() {
      match el {
        &OutputElement::Slice(sz) => acc += sz,
        _ => break,
      }
    }

    assert!(acc <= self.buffer.available_data(), "the merged output slices can't be larger than current data in buffer");
    acc
  }

  pub fn merge_output_deletes(&self) -> usize {
    let mut acc = 0usize;
    for el in self.output_queue.iter() {
      match el {
        &OutputElement::Delete(sz) => acc += sz,
        _ => break,
      }
    }

    assert!(acc <= self.buffer.available_data(), "the merged output deletes can't be larger than current data in buffer");
    acc
  }


  pub fn next_output_data(&self) -> &[u8] {
    let mut it = self.output_queue.iter();
    //first, calculate how many bytes we need to jump
    let mut start         = 0usize;
    let mut largest_size  = 0usize;
    let mut delete_ended  = false;
    for el in it {
      println!("start={}, length={}, el = {:?}", start, largest_size, el);
      if !delete_ended {
        match el {
          &OutputElement::Delete(sz) => start += sz,
          _ => {
            delete_ended = true;
            match el {
              &OutputElement::Slice(sz)     => largest_size += sz,
              &OutputElement::Insert(ref v) => return &v[..],
              _ => break,
            }
          },
        }
      } else {
        match el {
          &OutputElement::Slice(sz) => largest_size += sz,
          _ => break,
        }
      }
    }

    println!("buffer data: {:?}", self.buffer.data());
    println!("calculated start={}, length={}", start, largest_size);
    &self.buffer.data()[start..start+largest_size]
  }

  /// should only be called with a count inferior to self.input_data_size()
  pub fn consume_output_data(&mut self, size: usize) {
    let mut to_consume = size;
    while to_consume > 0 {
      let new_first_element = match self.output_queue.first() {
        None => {
          assert!(to_consume == 0, "no more element in queue, we should not ask to consume {} more bytes", to_consume);
          break;
        },
        Some(&OutputElement::Slice(sz)) => {
          self.buffer_position += to_consume;
          self.buffer.consume(to_consume);

          if to_consume >= sz {
            to_consume -= sz;
            None
          } else {
            let new_element = OutputElement::Slice(sz - to_consume);
            to_consume = 0;
            Some(new_element)
          }
        },
        Some(&OutputElement::Delete(sz)) => {
          self.buffer_position += sz;
          //FIXME: what if we can't delete that much data?
          self.buffer.consume(sz);
          None
        },
        Some(&OutputElement::Splice(sz)) => {
          if to_consume >= sz {
            to_consume -= sz;
            None
          } else {
            let new_element = OutputElement::Splice(sz - to_consume);
            to_consume = 0;
            Some(new_element)
          }
        },
        Some(&OutputElement::Insert(ref v)) => {
          if to_consume >= v.len() {
            to_consume = to_consume - v.len();
            None
          } else {
            let new_element = OutputElement::Insert(Vec::from(&v[to_consume..]));
            to_consume = 0;
            Some(new_element)
          }
        },
      };

      match new_first_element {
        None     => { self.output_queue.remove(0); },
        Some(el) => { self.output_queue[0] = el; },
      };
    }
  }
}

impl Write for BufferQueue {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    match self.buffer.write(buf) {
      Err(e) => Err(e),
      Ok(sz) => {
        self.input_queue.push(InputElement::Slice(sz));
        Ok(sz)
      }
    }
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

impl Reset for BufferQueue {
  fn reset(&mut self) {
    self.parsed_position = 0;
    self.output_position = 0;
    self.buffer_position = 0;
    self.start_parsing_position = 0;
    self.buffer.reset();
    self.input_queue.clear();
    self.output_queue.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use network::buffer::Buffer;
  use nom::HexDisplay;
  use std::io::Write;

  #[test]
  fn consume() {
    let mut b = BufferQueue {
      parsed_position:        0,
      output_position:        0,
      buffer_position:        0,
      start_parsing_position: 0,
      buffer:                 Buffer::from_slice(b"ABCDEFGHIJ"),
      input_queue:            vec!(InputElement::Slice(10)),
      output_queue:           vec!()
    };

    assert_eq!(b.unparsed_data(), &b"ABCDEFGHIJ"[..]);
    b.consume_parsed_data(4);
    assert_eq!(b.parsed_position, 4);
    assert_eq!(b.start_parsing_position, 4);
    assert_eq!(b.input_queue, vec!(InputElement::Slice(6)));
    println!("TEST[{}]", line!());
    assert_eq!(b.unparsed_data(), &b"EFGHIJ"[..]);
    println!("TEST[{}]", line!());

    b.slice_output(4);
    assert_eq!(b.output_queue, vec!(OutputElement::Slice(4)));

    b.insert_output(Vec::from(&b"test"[..]));
    assert_eq!(b.output_queue, vec!(
        OutputElement::Slice(4),
        OutputElement::Insert(Vec::from(&b"test"[..]))
      )
    );
    assert_eq!(b.next_output_data(), &b"ABCD"[..]);

    b.consume_output_data(2);
    assert_eq!(b.next_output_data(), &b"CD"[..]);

    println!("TEST[{}]", line!());
    b.consume_parsed_data(8);
    assert_eq!(b.parsed_position, 10);
    assert_eq!(b.start_parsing_position, 12);
    assert_eq!(b.input_queue, vec!());

    println!("TEST[{}]", line!());
    assert_eq!(b.unparsed_data(), &b""[..]);
    println!("TEST[{}]", line!());

    println!("**test**");
    b.consume_output_data(2);
    assert_eq!(b.next_output_data(), &b"test"[..]);
    b.consume_output_data(2);
    assert_eq!(b.next_output_data(), &b"st"[..]);

    b.delete_output(2);
    b.slice_output(4);
    assert_eq!(
      b.output_queue,
      vec!(
        OutputElement::Insert(Vec::from(&b"st"[..])),
        OutputElement::Delete(2),
        OutputElement::Slice(4)
      )
    );

    b.consume_output_data(2);
    assert_eq!(
      b.output_queue,
      vec!(
        OutputElement::Delete(2),
        OutputElement::Slice(4)
      )
    );
    assert_eq!(b.next_output_data(), &b"GHIJ"[..]);

    b.consume_output_data(1);
    assert_eq!(
      b.output_queue,
      vec!(
        OutputElement::Slice(3)
      )
    );
    assert_eq!(b.next_output_data(), &b"HIJ"[..]);

    b.write(&b"KLMNOP"[..]);
  }
}
