//! The jitter buffer: packages como from the network out of order and at unevent time; the speaker needs them in order 
//! , one every 20ms.


#[cfg(test)]
mod tests {
    use super::*;

    // The network may swap two packets: they come back in teh order they were sent.
    #[test]
    fn gives_packets_back_in_order() {
      let mut buffer = JitterBuffer::new();
      buffer.push(2, vec![2]);
      buffer.push(1, vec![1]);
      assert_eq!(buffer.pop(), Some(vec![1]));
      assert_eq!(buffer.pop(), Some(vec![2]));  

    }
  }

