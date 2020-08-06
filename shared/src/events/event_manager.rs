use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
    vec::Vec,
};

use crate::{
    entities::entity_type::EntityType,
    events::{
        event::{Event, EventClone},
        event_type::EventType,
    },
    manifest::Manifest,
    packet_reader::PacketReader,
};

/// Handles incoming/outgoing events, tracks the delivery status of Events so
/// that guaranteed Events can be re-transmitted to the remote host
#[derive(Debug)]
pub struct EventManager<T: EventType> {
    queued_outgoing_events: VecDeque<Rc<Box<dyn Event<T>>>>,
    queued_incoming_events: VecDeque<T>,
    sent_events: HashMap<u16, Vec<Rc<Box<dyn Event<T>>>>>,
}

impl<T: EventType> EventManager<T> {
    /// Creates a new EventManager
    pub fn new() -> Self {
        EventManager {
            queued_outgoing_events: VecDeque::new(),
            queued_incoming_events: VecDeque::new(),
            sent_events: HashMap::new(),
        }
    }

    /// Occurs when a packet has been notified as delivered. Stops tracking the
    /// status of Events in that packet.
    pub fn notify_packet_delivered(&mut self, packet_index: u16) {
        self.sent_events.remove(&packet_index);
    }

    /// Occurs when a packet has been notified as having been dropped. Queues up
    /// any guaranteed Events that were lost in the packet for retransmission.
    pub fn notify_packet_dropped(&mut self, packet_index: u16) {
        if let Some(dropped_events_list) = self.sent_events.get(&packet_index) {
            for dropped_event in dropped_events_list.into_iter() {
                self.queued_outgoing_events.push_back(dropped_event.clone());
            }

            self.sent_events.remove(&packet_index);
        }
    }

    /// Returns whether the Manager has queued Events that can be transmitted to
    /// the remote host
    pub fn has_outgoing_events(&self) -> bool {
        return self.queued_outgoing_events.len() != 0;
    }

    /// Gets the next queued Event to be transmitted
    pub fn pop_outgoing_event(&mut self, packet_index: u16) -> Option<Rc<Box<dyn Event<T>>>> {
        match self.queued_outgoing_events.pop_front() {
            Some(event) => {
                //place in transmission record if this is a gauranteed event
                if Event::is_guaranteed(event.as_ref().as_ref()) {
                    if !self.sent_events.contains_key(&packet_index) {
                        let sent_events_list: Vec<Rc<Box<dyn Event<T>>>> = Vec::new();
                        self.sent_events.insert(packet_index, sent_events_list);
                    }

                    if let Some(sent_events_list) = self.sent_events.get_mut(&packet_index) {
                        sent_events_list.push(event.clone());
                    }
                }

                Some(event)
            }
            None => None,
        }
    }

    /// If  the last popped Event from the queue somehow wasn't able to be
    /// written into a packet, put the Event back into the front of the queue
    pub fn unpop_outgoing_event(&mut self, packet_index: u16, event: &Rc<Box<dyn Event<T>>>) {
        let cloned_event = event.clone();

        if Event::is_guaranteed(event.as_ref().as_ref()) {
            if let Some(sent_events_list) = self.sent_events.get_mut(&packet_index) {
                sent_events_list.pop();
                if sent_events_list.len() == 0 {
                    self.sent_events.remove(&packet_index);
                }
            }
        }

        self.queued_outgoing_events.push_front(cloned_event);
    }

    /// Queues an Event to be transmitted to the remote host
    pub fn queue_outgoing_event(&mut self, event: &impl Event<T>) {
        let clone = Rc::new(EventClone::clone_box(event));
        self.queued_outgoing_events.push_back(clone);
    }

    /// Returns whether any Events have been received that must be handed to the
    /// application
    pub fn has_incoming_events(&self) -> bool {
        return self.queued_incoming_events.len() != 0;
    }

    /// Get the most recently received Event
    pub fn pop_incoming_event(&mut self) -> Option<T> {
        return self.queued_incoming_events.pop_front();
    }

    /// Given incoming packet data, read transmitted Events and store them to be
    /// returned to the application
    pub fn process_data<U: EntityType>(
        &mut self,
        reader: &mut PacketReader,
        manifest: &Manifest<T, U>,
    ) {
        let buffer = reader.get_buffer();
        let cursor = reader.get_cursor();

        let event_count = cursor.read_u8().unwrap();
        for _x in 0..event_count {
            let mut error_str: String = "not parsing? ".to_string();
            error_str += event_count.to_string().as_str();
            let naia_id: u16 = cursor.read_u16::<BigEndian>().expect(error_str.as_str());
            let payload_length: u8 = cursor.read_u8().unwrap().into();
            let payload_start_position: usize = cursor.position() as usize;
            let payload_end_position: usize = payload_start_position + (payload_length as usize);

            let event_payload = buffer[payload_start_position..payload_end_position]
                .to_vec()
                .into_boxed_slice();

            match manifest.create_event(naia_id, &event_payload) {
                Some(new_event) => {
                    //new_entity.read(&event_payload);
                    self.queued_incoming_events.push_back(new_event);
                }
                _ => {}
            }

            cursor.set_position(payload_end_position as u64);
        }
    }

    /// Write data into outgoing buffer
    pub fn write_data<U: EntityType>(
        manifest: &Manifest<T, U>,
        event: &Box<dyn Event<T>>,
    ) -> Vec<u8> {
        //Write event payload
        let mut event_payload_bytes = Vec::<u8>::new();
        event.as_ref().write(&mut event_payload_bytes);
        if event_payload_bytes.len() > 255 {
            error!("cannot encode an event with more than 255 bytes, need to implement this");
        }

        //Write event "header" (event id & payload length)
        let mut event_total_bytes = Vec::<u8>::new();

        let type_id = event.as_ref().get_type_id();
        let naia_id = manifest.get_event_naia_id(&type_id); // get naia id
        event_total_bytes.write_u16::<BigEndian>(naia_id).unwrap(); // write naia id
        event_total_bytes
            .write_u8(event_payload_bytes.len() as u8)
            .unwrap(); // write payload length
        event_total_bytes.append(&mut event_payload_bytes); // write payload

        return event_total_bytes;
    }
}
