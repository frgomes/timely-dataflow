use std::sync::mpsc::{Sender, Receiver, channel};
use std::marker::PhantomData;
use std::ops::Deref;

use serialization::Serializable;
use networking::networking::MessageHeader;

use communication::communicator::Process;
use communication::{Communicator, Data, Message, Pullable};

// TODO : wrap (usize, usize, usize) as a type?

// A communicator intended for binary channels (networking, pipes, shared memory)
pub struct Binary {
    pub inner:      Process,    // inner Process (use for process-local channels)
    pub index:      usize,                    // index of this worker
    pub peers:      usize,                    // number of peer workers
    pub graph:      usize,                    // identifier for the current graph
    pub allocated:  usize,                    // indicates how many channels have been allocated (locally).

    // for loading up state in the networking threads.
    pub writers:    Vec<Sender<((usize, usize, usize), Sender<Vec<u8>>)>>,
    pub readers:    Vec<Sender<((usize, usize, usize), (Sender<Vec<u8>>, Receiver<Vec<u8>>))>>,
    pub senders:    Vec<Sender<(MessageHeader, Vec<u8>)>>
}

impl Binary {
    pub fn inner<'a>(&'a mut self) -> &'a mut Process { &mut self.inner }
}

// A Communicator backed by Sender<Vec<u8>>/Receiver<Vec<u8>> pairs (e.g. networking, shared memory, files, pipes)
impl Communicator for Binary {
    fn index(&self) -> usize { self.index }
    fn peers(&self) -> usize { self.peers }
    fn new_channel<T:Data+Serializable, D: Data+Serializable>(&mut self) -> (Vec<::communication::observer::BoxedObserver<T, D>>, Box<Pullable<T, D>>) {
        let mut pushers: Vec<::communication::observer::BoxedObserver<T, D>> = Vec::new(); // built-up vector of BoxedObserver<T, D> to return

        // we'll need process-local channels as well (no self-loop binary connection in this design; perhaps should allow)
        let inner_peers = self.inner.peers();
        let (inner_sends, inner_recv) = self.inner.new_channel();

        // prep a pushable for each endpoint, multiplied by inner_peers
        for (index, writer) in self.writers.iter().enumerate() {
            for counter in (0..inner_peers) {
                let (s,_r) = channel();  // TODO : Obviously this should be deleted...
                let mut target_index = index * inner_peers + counter;

                // we may need to increment target_index by inner_peers;
                if index >= self.index / inner_peers { target_index += inner_peers; }

                writer.send(((self.index, self.graph, self.allocated), s)).unwrap();
                let header = MessageHeader {
                    graph:      self.graph,     // should be
                    channel:    self.allocated, //
                    source:     self.index,     //
                    target:     target_index,   //
                    length:     0,
                };
                pushers.push(::communication::observer::BoxedObserver::new(Observer::new(header, self.senders[index].clone())));
            }
        }

        // splice inner_sends into the vector of pushables
        for (index, writer) in inner_sends.into_iter().enumerate() {
            pushers.insert(((self.index / inner_peers) * inner_peers) + index, writer);
        }

        // prep a Box<Pullable<T>> using inner_recv and fresh registered pullables
        let (send,recv) = channel();    // binary channel from binary listener to BinaryPullable<T>
        let mut pullsends = Vec::new();
        for reader in self.readers.iter() {
            let (s,r) = channel();
            pullsends.push(s);
            reader.send(((self.index, self.graph, self.allocated), (send.clone(), r))).unwrap();
        }

        let pullable = Box::new(BinaryPullable::new(inner_recv, recv));

        self.allocated += 1;

        return (pushers, pullable);
    }
}

struct Observer<T, D> {
    header:     MessageHeader,
    sender:     Sender<(MessageHeader, Vec<u8>)>,   // targets for each remote destination
    phantom:    PhantomData<D>,
    time: Option<T>,
}

impl<T, D> Observer<T, D> {
    pub fn new(header: MessageHeader, sender: Sender<(MessageHeader, Vec<u8>)>) -> Observer<T, D> {
        Observer {
            header:     header,
            sender:     sender,
            phantom:    PhantomData,
            time: None,
        }
    }
}

impl<T:Data+Serializable, D:Data+Serializable> ::communication::observer::Observer for Observer<T, D> {
    type Time = T;
    type Data = D;

    #[inline] fn open(&mut self, time: &Self::Time) {
        assert!(self.time.is_none());
        self.time = Some(time.clone());
    }
    #[inline] fn shut(&mut self,_time: &Self::Time) {
        assert!(self.time.is_some());
        self.time = None;
    }
    #[inline] fn give(&mut self, data: &mut Message<Self::Data>) {
        assert!(self.time.is_some());
        if data.len() > 0 {
            if let Some(time) = self.time.clone() {
                // TODO : anything better to do here than allocate (bytes)?
                // TODO : perhaps team up with the Pushable to recycle (bytes) ...
                // ALLOC : We create some new byte buffers here, because we have to.
                // ALLOC : We would love to borrow these from somewhere nearby, if possible.
                let mut bytes = Vec::new();
                Serializable::encode(&time, &mut bytes);
                let vec: &Vec<D> = data.deref();
                Serializable::encode(vec, &mut bytes);

                // NOTE : We do not .clear() data, because that could forcibly allocate.
                // NOTE : Instead, upstream folks are expected to clear allocations before re-using.

                let mut header = self.header;
                header.length = bytes.len();

                self.sender.send((header, bytes)).ok();
            }
        }
    }
}

struct BinaryPullable<T, D> {
    inner: Box<Pullable<T, D>>,       // inner pullable (e.g. intra-process typed queue)
    current: Option<(T, Message<D>)>,
    receiver: Receiver<Vec<u8>>,      // source of serialized buffers
}
impl<T:Data+Serializable, D: Data+Serializable> BinaryPullable<T, D> {
    fn new(inner: Box<Pullable<T, D>>, receiver: Receiver<Vec<u8>>) -> BinaryPullable<T, D> {
        BinaryPullable {
            inner: inner,
            current: None,
            receiver: receiver,
        }
    }
}

impl<T:Data+Serializable, D: Data+Serializable> Pullable<T, D> for BinaryPullable<T, D> {
    #[inline]
    fn pull(&mut self) -> Option<(&T, &mut Message<D>)> {
        if let Some(pair) = self.inner.pull() { Some(pair) }
        else {
            // TODO : Do something better than drop self.current
            self.current = self.receiver.try_recv().ok().map(|mut bytes| {
                let x_len = bytes.len();
                let (time, offset) = {
                    let (t,r) = <T as Serializable>::decode(&mut bytes).unwrap();
                    let o = x_len - r.len();
                    ((*t).clone(), o)
                };

                (time, Message::from_bytes(bytes, offset))
            });

            if let Some((_, ref message)) = self.current {
                assert!(message.len() > 0);
            }
            self.current.as_mut().map(|&mut (ref time, ref mut data)| (time, data))
        }
    }
}