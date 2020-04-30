mod ll;

use self::ll::{LLNode, LL};
use crate::collections::bptree::*;
use parking_lot::{Mutex, MutexGuard};
use std::collections::BTreeMap;
use std::collections::BinaryHeap;
use std::sync::mpsc::{channel, Receiver, Sender};

use std::borrow::{Borrow, ToOwned};
use std::cell::UnsafeCell;
use std::fmt::Debug;
use std::hash::Hash;
use std::mem;
use std::time::Instant;

struct ThreadCacheItem<V> {
    v: V,
    clean: bool,
}

enum CacheEvent<K, V> {
    Hit(Instant, K),
    Include(Instant, K, V, usize),
}

#[derive(Clone, Debug)]
enum CacheType<K, V>
where
    K: Hash + Eq + Ord + Clone + Debug,
{
    Freq(*mut LLNode<K>, V),
    Rec(*mut LLNode<K>, V),
    GhostFreq(*mut LLNode<K>),
    GhostRec(*mut LLNode<K>),
    Haunted(*mut LLNode<K>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CacheState {
    Freq,
    Rec,
    GhostFreq,
    GhostRec,
    Haunted,
    None,
}

#[derive(Clone, Debug)]
struct CacheItem<K, V>
where
    K: Hash + Eq + Ord + Clone + Debug,
    V: Clone + Debug,
{
    // We need to store k in the cache item, because all values need it, and we need to
    // mutate in place the txid. It also allows past txn's to do lookups, because the *mut
    // llnodes *may* havebeen freed, so it's unsafe to access t in a reader. But in the writer
    // it is safe, but each node of the llnode needs it's k as well so that on a remove of the item
    // we can properly change and affect the value.
    // k: K,
    txid: usize,
    t: CacheType<K, V>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct CStat {
    max: usize,
    cache: usize,
    tlocal: usize,
    freq: usize,
    rec: usize,
    ghost_freq: usize,
    ghost_rec: usize,
    haunted: usize,
    p: usize,
}

struct ArcInner<K, V>
where
    K: Hash + Eq + Ord + Clone + Debug,
    V: Clone + Debug,
{
    // Weight of items between the two caches.
    p: usize,
    freq: LL<K>,
    rec: LL<K>,
    ghost_freq: LL<K>,
    ghost_rec: LL<K>,
    haunted: LL<K>,
    rx: Receiver<CacheEvent<K, V>>,
}

pub struct Arc<K, V>
where
    K: Hash + Eq + Ord + Clone + Debug,
    V: Clone + Debug,
{
    // Use a unified tree, allows simpler movement of items between the
    // cache types.
    cache: BptreeMap<K, CacheItem<K, V>>,
    // Max number of elements to cache.
    max: usize,
    // channels for readers.
    // tx (cloneable)
    tx: Sender<CacheEvent<K, V>>,
    inner: Mutex<ArcInner<K, V>>,
}

pub struct ArcReadTxn<K, V>
where
    K: Hash + Eq + Ord + Clone + Debug,
    V: Clone + Debug,
{
    // ro_txn to cache
    cache: BptreeMapReadTxn<K, CacheItem<K, V>>,
    // cache of our missed items to send forward.
    // On drop we drain this to the channel
    tlocal: BTreeMap<K, ThreadCacheItem<V>>,
    // tx channel to send forward events.
    tx: Sender<CacheEvent<K, V>>,
    ts: Instant,
}

pub struct ArcWriteTxn<'a, K, V>
where
    K: Hash + Eq + Ord + Clone + Debug,
    V: Clone + Debug,
{
    caller: &'a Arc<K, V>,
    // wr_txn to cache
    cache: BptreeMapWriteTxn<'a, K, CacheItem<K, V>>,
    // Cache of missed items (w_ dirty/clean)
    // On COMMIT we drain this to the main cache
    tlocal: BTreeMap<K, ThreadCacheItem<V>>,
    hit: UnsafeCell<Vec<K>>,
}

/*
pub struct ArcReadSnapshot<K, V> {
    // How to communicate back to the caller the loads we did?
    tlocal: &mut BTreeMap<K, ThreadCacheItem<K, V>>,
}
*/

impl<K: Hash + Eq + Ord + Clone + Debug, V: Clone + Debug> CacheItem<K, V> {
    fn to_vref(&self) -> Option<&V> {
        match &self.t {
            CacheType::Freq(_, v) | CacheType::Rec(_, v) => Some(&v),
            _ => None,
        }
    }

    fn to_state(&self) -> CacheState {
        match &self.t {
            CacheType::Freq(_, _v) => CacheState::Freq,
            CacheType::Rec(_, _v) => CacheState::Rec,
            CacheType::GhostFreq(_) => CacheState::GhostFreq,
            CacheType::GhostRec(_) => CacheState::GhostRec,
            CacheType::Haunted(_) => CacheState::Haunted,
        }
    }
}

macro_rules! evict_to_len {
    (
        $cache:expr,
        $ll:expr,
        $to_ll:expr,
        $size:expr,
        $txid:expr
    ) => {{
        debug_assert!($ll.len() >= $size);

        while $ll.len() > $size {
            let n = $ll.pop();
            debug_assert!(!n.is_null());
            let mut r = $cache.get_mut(unsafe { &(*n).k });
            match r {
                Some(ref mut ci) => {
                    let mut next_state = match &ci.t {
                        CacheType::Freq(llp, _v) => {
                            debug_assert!(*llp == n);
                            // No need to extract, already popped!
                            // $ll.extract(*llp);
                            $to_ll.append_n(*llp);
                            CacheType::GhostFreq(*llp)
                        }
                        CacheType::Rec(llp, _v) => {
                            debug_assert!(*llp == n);
                            // No need to extract, already popped!
                            // $ll.extract(*llp);
                            $to_ll.append_n(*llp);
                            CacheType::GhostRec(*llp)
                        }
                        _ => {
                            // Impossible state!
                            unreachable!();
                        }
                    };
                    // Now change the state.
                    mem::swap(&mut ci.t, &mut next_state);
                    // And set the item's evict txid.
                    ci.txid = $txid;
                }
                None => {
                    // Impossible state!
                    unreachable!();
                }
            }
        }
    }};
}

macro_rules! evict_to_haunted_len {
    (
        $cache:expr,
        $ll:expr,
        $to_ll:expr,
        $size:expr,
        $txid:expr
    ) => {{
        debug_assert!($ll.len() >= $size);

        while $ll.len() > $size {
            let n = $ll.pop();
            debug_assert!(!n.is_null());
            $to_ll.append_n(n);
            let mut r = $cache.get_mut(unsafe { &(*n).k });
            match r {
                Some(ref mut ci) => {
                    // Now change the state.
                    let mut next_state = CacheType::Haunted(n);
                    mem::swap(&mut ci.t, &mut next_state);
                    // And set the item's evict txid.
                    ci.txid = $txid;
                }
                None => {
                    // Impossible state!
                    unreachable!();
                }
            };
        }
    }};
}

impl<K: Hash + Eq + Ord + Clone + Debug, V: Clone + Debug> Arc<K, V> {
    pub fn new(max: usize) -> Self {
        let (tx, rx) = channel();
        let inner = Mutex::new(ArcInner {
            p: 0,
            freq: LL::new(),
            rec: LL::new(),
            ghost_freq: LL::new(),
            ghost_rec: LL::new(),
            haunted: LL::new(),
            rx,
        });
        Arc {
            cache: BptreeMap::new(),
            max,
            tx,
            inner,
        }
    }

    pub fn read(&self) -> ArcReadTxn<K, V> {
        ArcReadTxn {
            cache: self.cache.read(),
            tlocal: BTreeMap::new(),
            tx: self.tx.clone(),
            ts: Instant::now(),
        }
    }

    pub fn write(&self) -> ArcWriteTxn<K, V> {
        ArcWriteTxn {
            caller: &self,
            cache: self.cache.write(),
            tlocal: BTreeMap::new(),
            hit: UnsafeCell::new(Vec::new()),
        }
    }

    fn try_write(&self) -> Option<ArcWriteTxn<K, V>> {
        self.cache.try_write().map(|cache| ArcWriteTxn {
            caller: &self,
            cache: cache,
            tlocal: BTreeMap::new(),
            hit: UnsafeCell::new(Vec::new()),
        })
    }

    pub fn try_quiesce(&self) {
        match self.try_write() {
            Some(wr_txn) => wr_txn.commit(),
            None => {}
        }
    }

    fn calc_p_freq(ghost_rec_len: usize, ghost_freq_len: usize, p: &mut usize) {
        let delta = if ghost_rec_len > ghost_freq_len {
            ghost_rec_len / ghost_freq_len
        } else {
            1
        };
        if delta < *p {
            *p -= delta
        } else {
            *p = 0
        }
    }

    fn calc_p_rec(cap: usize, ghost_rec_len: usize, ghost_freq_len: usize, p: &mut usize) {
        let delta = if ghost_freq_len > ghost_rec_len {
            ghost_freq_len / ghost_rec_len
        } else {
            1
        };
        if delta <= cap - *p {
            *p += delta
        } else {
            *p = cap
        }
    }

    fn commit<'a>(
        &'a self,
        mut cache: BptreeMapWriteTxn<'a, K, CacheItem<K, V>>,
        tlocal: BTreeMap<K, ThreadCacheItem<V>>,
        hit: Vec<K>,
    ) {
        // What is the time?
        let commit_ts = Instant::now();
        let commit_txid = cache.get_txid();
        // Copy p + init cache sizes for adjustment.
        let mut inner = self.inner.lock();

        // Why is it okay to drain the rx/tlocal and create the cache in a temporary
        // oversize? Because these values in the queue/tlocal are already in memory
        // and we are moving them to the cache, we are not actually using any more
        // memory (well, not significantly more). By adding everything, then evicting
        // we also get better and more accurate hit patterns over the cache based on what
        // was used. This gives us an advantage over other cache types - we can see
        // patterns based on temporal usage that other caches can't, at the expense that
        // it may take some moments for that cache pattern to sync to the main thread.

        // drain tlocal into the main cache.
        tlocal.into_iter().for_each(|(k, tci)| {
            // * everything in tlocal is dirty or include.
            let mut r = cache.get_mut(&k);
            match r {
                Some(ref mut ci) => {
                    //   * as we include each item, what state was it in before?
                    // It's in the cache - what action must we take?
                    let mut next_state = match &ci.t {
                        CacheType::Freq(llp, _v) => {
                            // println!("tlocal {:?} Freq -> Freq", k);
                            // Move the list item to it's head.
                            inner.freq.touch(*llp);
                            // Update v.
                            CacheType::Freq(*llp, tci.v)
                        }
                        CacheType::Rec(llp, _v) => {
                            // println!("tlocal {:?} Rec -> Freq", k);
                            // Remove the node and put it into freq.
                            inner.rec.extract(*llp);
                            inner.freq.append_n(*llp);
                            CacheType::Freq(*llp, tci.v)
                        }
                        CacheType::GhostFreq(llp) => {
                            // println!("tlocal {:?} GhostFreq -> Freq", k);
                            // Ajdust p
                            Self::calc_p_freq(
                                inner.ghost_rec.len(),
                                inner.ghost_freq.len(),
                                &mut inner.p,
                            );
                            inner.ghost_freq.extract(*llp);
                            inner.freq.append_n(*llp);
                            CacheType::Freq(*llp, tci.v)
                        }
                        CacheType::GhostRec(llp) => {
                            // println!("tlocal {:?} GhostRec -> Rec", k);
                            // Ajdust p
                            Self::calc_p_rec(
                                self.max,
                                inner.ghost_rec.len(),
                                inner.ghost_freq.len(),
                                &mut inner.p,
                            );
                            inner.ghost_rec.extract(*llp);
                            inner.rec.append_n(*llp);
                            CacheType::Rec(*llp, tci.v)
                        }
                        CacheType::Haunted(llp) => {
                            // println!("tlocal {:?} Haunted -> Rec", k);
                            inner.haunted.extract(*llp);
                            inner.rec.append_n(*llp);
                            CacheType::Rec(*llp, tci.v)
                        }
                    };
                    // Now change the state.
                    mem::swap(&mut ci.t, &mut next_state);
                    // And touch the txid.
                    ci.txid = commit_txid;
                }
                None => {
                    // println!("tlocal {:?} None -> Rec", k);
                    // It's not present - include it!
                    let llp = inner.rec.append_k(k.clone());
                    cache.insert(
                        k,
                        CacheItem {
                            txid: commit_txid,
                            t: CacheType::Rec(llp, tci.v),
                        },
                    );
                }
            }
            //   * does this affect p?
        });

        // drain rx until empty or time >= time.
        // * for each item
        while let Ok(ce) = inner.rx.try_recv() {
            let t = match ce {
                // Update if it was hit.
                CacheEvent::Hit(t, k) => {
                    let mut r = cache.get_mut(&k);
                    match r {
                        Some(ref mut ci) => {
                            let mut next_state = match &ci.t {
                                CacheType::Freq(llp, v) => {
                                    // println!("rxhit {:?} Freq -> Freq", k);
                                    inner.freq.touch(*llp);
                                    CacheType::Freq(*llp, v.clone())
                                }
                                CacheType::Rec(llp, v) => {
                                    // println!("rxhit {:?} Rec -> Freq", k);
                                    inner.rec.extract(*llp);
                                    inner.freq.append_n(*llp);
                                    CacheType::Freq(*llp, v.clone())
                                }
                                // While we can't add this from nothing, we can
                                // at least keep it in the ghost sets.
                                CacheType::GhostFreq(llp) => {
                                    // println!("rxhit {:?} GhostFreq -> GhostFreq", k);
                                    inner.ghost_freq.touch(*llp);
                                    CacheType::GhostFreq(*llp)
                                }
                                CacheType::GhostRec(llp) => {
                                    // println!("rxhit {:?} GhostRec -> GhostRec", k);
                                    inner.ghost_rec.touch(*llp);
                                    CacheType::GhostRec(*llp)
                                }
                                CacheType::Haunted(llp) => {
                                    // println!("rxhit {:?} Haunted -> Haunted", k);
                                    // We can't do anything about this ...
                                    CacheType::Haunted(*llp)
                                }
                            };
                            mem::swap(&mut ci.t, &mut next_state);
                        }
                        None => {
                            // Do nothing, it must have been evicted.
                        }
                    };
                    t
                }
                // Update if it was inc
                CacheEvent::Include(t, k, iv, txid) => {
                    let mut r = cache.get_mut(&k);
                    match r {
                        Some(ref mut ci) => {
                            let mut next_state = match &ci.t {
                                CacheType::Freq(llp, v) => {
                                    inner.freq.touch(*llp);
                                    if ci.txid >= txid {
                                        // println!("rxinc {:?} Freq -> Freq (touch only)", k);
                                        // Our cache already has a newer value, keep it.
                                        CacheType::Freq(*llp, v.clone())
                                    } else {
                                        // println!("rxinc {:?} Freq -> Freq (update)", k);
                                        // The value is newer, update.
                                        ci.txid = txid;
                                        CacheType::Freq(*llp, iv)
                                    }
                                }
                                CacheType::Rec(llp, v) => {
                                    inner.rec.extract(*llp);
                                    inner.freq.append_n(*llp);
                                    if ci.txid >= txid {
                                        // println!("rxinc {:?} Rec -> Freq (touch only)", k);
                                        CacheType::Freq(*llp, v.clone())
                                    } else {
                                        // println!("rxinc {:?} Rec -> Freq (update)", k);
                                        ci.txid = txid;
                                        CacheType::Freq(*llp, iv)
                                    }
                                }
                                CacheType::GhostFreq(llp) => {
                                    // Adjust p
                                    Self::calc_p_freq(
                                        inner.ghost_rec.len(),
                                        inner.ghost_freq.len(),
                                        &mut inner.p,
                                    );
                                    inner.ghost_freq.extract(*llp);
                                    if ci.txid > txid {
                                        // println!("rxinc {:?} GhostFreq -> GhostFreq", k);
                                        // The cache version is newer, this is just a hit.
                                        inner.ghost_freq.append_n(*llp);
                                        CacheType::GhostFreq(*llp)
                                    } else {
                                        // This item is newer, so we can include it.
                                        // println!("rxinc {:?} GhostFreq -> Rec", k);
                                        inner.freq.append_n(*llp);
                                        ci.txid = txid;
                                        CacheType::Freq(*llp, iv)
                                    }
                                }
                                CacheType::GhostRec(llp) => {
                                    // Adjust p
                                    Self::calc_p_rec(
                                        self.max,
                                        inner.ghost_rec.len(),
                                        inner.ghost_freq.len(),
                                        &mut inner.p,
                                    );
                                    inner.ghost_rec.extract(*llp);
                                    if ci.txid > txid {
                                        // println!("rxinc {:?} GhostRec -> GhostRec", k);
                                        inner.ghost_rec.append_n(*llp);
                                        CacheType::GhostRec(*llp)
                                    } else {
                                        // println!("rxinc {:?} GhostRec -> Rec", k);
                                        inner.rec.append_n(*llp);
                                        ci.txid = txid;
                                        CacheType::Rec(*llp, iv)
                                    }
                                }
                                CacheType::Haunted(llp) => {
                                    if ci.txid > txid {
                                        // println!("rxinc {:?} Haunted -> Haunted", k);
                                        CacheType::Haunted(*llp)
                                    } else {
                                        // println!("rxinc {:?} Haunted -> Rec", k);
                                        inner.haunted.extract(*llp);
                                        inner.rec.append_n(*llp);
                                        ci.txid = txid;
                                        CacheType::Rec(*llp, iv)
                                    }
                                }
                            };
                            mem::swap(&mut ci.t, &mut next_state);
                        }
                        None => {
                            // It's not present - include it!
                            // println!("rxinc {:?} None -> Rec", k);
                            let llp = inner.rec.append_k(k.clone());
                            cache.insert(
                                k,
                                CacheItem {
                                    txid,
                                    t: CacheType::Rec(llp, iv),
                                },
                            );
                        }
                    };
                    t
                }
            };
            // Stop processing the queue, we are up to "now".
            if t >= commit_ts {
                break;
            }
        }

        // drain the tlocal hits into the main cache.
        hit.into_iter().for_each(|k| {
            // * everything hit must be in main cache now, so bring these
            //   all to the relevant item heads.
            // * Why do this last? Because the write is the "latest" we want all the fresh
            //   written items in the cache over the "read" hits, it gives us some aprox
            //   of time ordering, but not perfect.

            // Find the item in the cache.
            // * based on it's type, promote it in the correct list, or move it.
            // How does this prevent incorrect promotion from rec to freq? txid?
            let mut r = cache.get_mut(&k);
            match r {
                Some(ref mut ci) => {
                    // This differs from above - we skip if we don't touch anything
                    // that was added in this txn. This is to prevent double touching
                    // anything that was included in a write.
                    if ci.txid != commit_txid {
                        let mut next_state = match &ci.t {
                            CacheType::Freq(llp, v) => {
                                // println!("hit {:?} Freq -> Freq", k);
                                inner.freq.touch(*llp);
                                CacheType::Freq(*llp, v.clone())
                            }
                            CacheType::Rec(llp, v) => {
                                // println!("hit {:?} Rec -> Freq", k);
                                inner.rec.extract(*llp);
                                inner.freq.append_n(*llp);
                                CacheType::Freq(*llp, v.clone())
                            }
                            _ => {
                                // Impossible state!
                                unreachable!();
                            }
                        };
                        // Now change the state.
                        mem::swap(&mut ci.t, &mut next_state);
                    }
                }
                None => {
                    // Impossible state!
                    unreachable!();
                }
            }
        });

        // now clean the space for each of the primary caches, evicting into the ghost sets.
        // * It's possible that both caches are now over-sized if rx was empty
        //   but wr inc many items.
        // * p has possibly changed width, causing a balance shift
        // * and ghost items have been included changing ghost list sizes.
        // so we need to do a clean up/balance of all the list lengths.
        debug_assert!(inner.p <= self.max);
        // Convince the compiler copying is okay.
        let p = inner.p;

        if inner.rec.len() + inner.freq.len() > self.max {
            // println!("Checking cache evict");
            /*
            println!(
                "from -> rec {:?}, freq {:?}",
                inner.rec.len(),
                inner.freq.len()
            );
            */
            let delta = (inner.rec.len() + inner.freq.len()) - self.max;
            // We have overflowed by delta. As we are not "evicting as we go" we have to work out
            // what we should have evicted up to now.
            //
            // keep removing from rec until == p OR delta == 0, and if delta remains, then remove from freq.

            let rec_to_len = if inner.p == 0 {
                // println!("p == 0 => {:?}", inner.rec.len());
                debug_assert!(delta <= inner.rec.len());
                // We are fully weight to freq, so only remove in rec.
                inner.rec.len() - delta
            } else if inner.rec.len() > inner.p {
                // There is a partial weighting, how much do we need to move?
                let rec_delta = inner.rec.len() - inner.p;
                if rec_delta > delta {
                    /*
                    println!(
                        "p ({:?}) <= rec ({:?}), rec_delta ({:?}) > delta ({:?})",
                        inner.p,
                        inner.rec.len(),
                        rec_delta,
                        delta
                    );
                    */
                    // We will have removed enough through delta alone in rec.
                    inner.rec.len() - delta
                } else {
                    /*
                    println!(
                        "p ({:?}) <= rec ({:?}), rec_delta ({:?}) <= delta ({:?})",
                        inner.p,
                        inner.rec.len(),
                        rec_delta,
                        delta
                    );
                    */
                    // Remove the full delta, and excess will be removed from freq.
                    inner.rec.len() - rec_delta
                }
            } else {
                // rec is already below p, therefore we must need to remove in freq, and
                // we need to consider how much is in rec.
                // println!("p ({:?}) > rec ({:?})", inner.p, inner.rec.len());
                inner.rec.len()
            };

            // Now we can get the expected sizes;
            debug_assert!(self.max >= rec_to_len);
            let freq_to_len = self.max - rec_to_len;
            // println!("move to -> rec {:?}, freq {:?}", rec_to_len, freq_to_len);
            debug_assert!(freq_to_len + rec_to_len <= self.max);

            evict_to_len!(
                &mut cache,
                &mut inner.rec,
                &mut inner.ghost_rec,
                rec_to_len,
                commit_txid
            );
            evict_to_len!(
                &mut cache,
                &mut inner.freq,
                &mut inner.ghost_freq,
                freq_to_len,
                commit_txid
            );

            // Finally, do an evict of the ghost sets if they are too long - these are weighted
            // inverse to the above sets.
            if inner.ghost_rec.len() > (self.max - p) {
                evict_to_haunted_len!(
                    &mut cache,
                    &mut inner.ghost_rec,
                    &mut inner.haunted,
                    freq_to_len,
                    commit_txid
                );
            }

            if inner.ghost_freq.len() > p {
                evict_to_haunted_len!(
                    &mut cache,
                    &mut inner.ghost_freq,
                    &mut inner.haunted,
                    rec_to_len,
                    commit_txid
                );
            }
        }

        // commit on the wr txn.
        cache.commit();
        // done!
    }
}

impl<'a, K: Hash + Eq + Ord + Clone + Debug, V: Clone + Debug> ArcWriteTxn<'a, K, V> {
    pub fn commit(self) {
        self.caller
            .commit(self.cache, self.tlocal, self.hit.into_inner())
    }

    // get
    pub fn get<'b, Q: ?Sized>(&'a self, k: &'b Q) -> Option<&'a V>
    where
        K: Borrow<Q> + From<Q>,
        Q: Hash + Eq + Ord + Clone,
    {
        let r: Option<&V> = if let Some(tci) = self.tlocal.get(k) {
            let v = &tci.v as *const _;
            unsafe { Some(&(*v)) }
        } else {
            if let Some(v) = self.cache.get(k) {
                v as *const _;
                unsafe { (*v).to_vref() }
            } else {
                None
            }
        };
        // How do we track this was a hit?
        // Remember, we don't track misses - they are *implied* by the fact they'll trigger
        // an inclusion from the external system. Subsequent, any further re-hit on an
        // included value WILL be tracked, allowing arc to adjust appropriately.
        if r.is_some() {
            let hk: K = k.to_owned().into();
            unsafe {
                let hit_ptr = self.hit.get();
                (*hit_ptr).push(hk);
            }
        }
        r
    }

    // contains
    pub fn contains_key<'b, Q: ?Sized>(&'a self, k: &'b Q) -> bool
    where
        K: Borrow<Q> + From<Q>,
        Q: Hash + Eq + Ord + Clone,
    {
        self.get(k).is_some()
    }

    // insert
    // Add this value to a cache. For us that just means the tlocal cache.
    pub fn insert(&mut self, k: K, v: V) {
        self.tlocal.insert(k, ThreadCacheItem { v, clean: true });
    }

    pub(crate) fn peek_hit(&self) -> &[K] {
        let hit_ptr = self.hit.get();
        unsafe { &(*hit_ptr) }
    }

    pub(crate) fn peek_cache<'b, Q: ?Sized>(&'a self, k: &'b Q) -> CacheState
    where
        K: Borrow<Q> + From<Q>,
        Q: Hash + Eq + Ord + Clone,
    {
        if let Some(v) = self.cache.get(k) {
            v as *const _;
            unsafe { (*v).to_state() }
        } else {
            CacheState::None
        }
    }

    pub(crate) fn peek_stat(&self) -> CStat {
        let inner = self.caller.inner.lock();
        CStat {
            max: self.caller.max,
            cache: self.cache.len(),
            tlocal: self.tlocal.len(),
            freq: inner.freq.len(),
            rec: inner.rec.len(),
            ghost_freq: inner.ghost_freq.len(),
            ghost_rec: inner.ghost_rec.len(),
            haunted: inner.haunted.len(),
            p: inner.p,
        }
    }

    // get_mut
    //  If it's in tlocal, return that as get_mut
    // if it's in the cache, clone to tlocal, then get_mut to tlock
    // if not, return none.

    // to_snapshot
}

impl<K: Hash + Eq + Ord + Clone + Debug, V: Clone + Debug> ArcReadTxn<K, V> {
    pub fn get<'b, Q: ?Sized>(&'b self, k: &'b Q) -> Option<&'b V>
    where
        K: Borrow<Q> + From<Q>,
        Q: Hash + Eq + Ord + Clone,
    {
        let r: Option<&V> = if let Some(tci) = self.tlocal.get(k) {
            let v = &tci.v as *const _;
            unsafe { Some(&(*v)) }
        } else {
            if let Some(v) = self.cache.get(k) {
                v as *const _;
                unsafe { (*v).to_vref() }
            } else {
                None
            }
        };

        if r.is_some() {
            let hk: K = k.to_owned().into();
            self.tx
                .send(CacheEvent::Hit(self.ts.clone(), hk))
                .expect("Invalid tx state");
        }
        r
    }

    pub fn contains_key<'b, Q: ?Sized>(&self, k: &'b Q) -> bool
    where
        K: Borrow<Q> + From<Q>,
        Q: Hash + Eq + Ord + Clone,
    {
        self.get(k).is_some()
    }

    pub fn insert(&mut self, k: K, v: V) {
        // Send a copy forward through time and space.
        self.tx
            .send(CacheEvent::Include(
                self.ts.clone(),
                k.clone(),
                v.clone(),
                self.cache.get_txid(),
            ))
            .expect("Invalid tx state!");
        self.tlocal.insert(k, ThreadCacheItem { v, clean: true });
    }
}

#[cfg(test)]
mod tests {
    use crate::cache::arc::Arc;
    use crate::cache::arc::CStat;
    use crate::cache::arc::CacheState;

    #[test]
    fn test_cache_arc_basic() {
        let arc: Arc<usize, usize> = Arc::new(4);
        let mut wr_txn = arc.write();

        assert!(wr_txn.get(&1) == None);
        assert!(wr_txn.peek_hit().len() == 0);
        wr_txn.insert(1, 1);
        assert!(wr_txn.get(&1) == Some(&1));
        assert!(wr_txn.peek_hit().len() == 1);

        wr_txn.commit();

        // Now we start the second txn, and see if it's in there.
        let mut wr_txn = arc.write();
        assert!(wr_txn.peek_cache(&1) == CacheState::Rec);
        assert!(wr_txn.get(&1) == Some(&1));
        assert!(wr_txn.peek_hit().len() == 1);
        wr_txn.commit();
        // And now check it's moved to Freq due to the extra
        let mut wr_txn = arc.write();
        assert!(wr_txn.peek_cache(&1) == CacheState::Freq);
        println!("{:?}", wr_txn.peek_stat());
    }

    #[test]
    fn test_cache_evict() {
        println!("== 1");
        let arc: Arc<usize, usize> = Arc::new(4);
        let mut wr_txn = arc.write();
        assert!(
            CStat {
                max: 4,
                cache: 0,
                tlocal: 0,
                freq: 0,
                rec: 0,
                ghost_freq: 0,
                ghost_rec: 0,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );

        // In the first txn we insert 4 items.
        wr_txn.insert(1, 1);
        wr_txn.insert(2, 2);
        wr_txn.insert(3, 3);
        wr_txn.insert(4, 4);

        assert!(
            CStat {
                max: 4,
                cache: 0,
                tlocal: 4,
                freq: 0,
                rec: 0,
                ghost_freq: 0,
                ghost_rec: 0,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        wr_txn.commit();

        // Now we start the second txn, and check the stats.
        println!("== 2");
        let mut wr_txn = arc.write();
        assert!(
            CStat {
                max: 4,
                cache: 4,
                tlocal: 0,
                freq: 0,
                rec: 4,
                ghost_freq: 0,
                ghost_rec: 0,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );

        // Now touch two items, this promote to the freq set.
        // Remember, a double hit doesn't weight any more than 1 hit.
        assert!(wr_txn.get(&1) == Some(&1));
        assert!(wr_txn.get(&1) == Some(&1));
        assert!(wr_txn.get(&2) == Some(&2));

        wr_txn.commit();

        // Now we start the third txn, and check the stats.
        println!("== 3");
        let mut wr_txn = arc.write();
        assert!(
            CStat {
                max: 4,
                cache: 4,
                tlocal: 0,
                freq: 2,
                rec: 2,
                ghost_freq: 0,
                ghost_rec: 0,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        // Add one more item, this will trigger an evict.
        wr_txn.insert(5, 5);
        wr_txn.commit();

        // Now we start the fourth txn, and check the stats.
        println!("== 4");
        let mut wr_txn = arc.write();
        println!("stat -> {:?}", wr_txn.peek_stat());
        assert!(
            CStat {
                max: 4,
                cache: 5,
                tlocal: 0,
                freq: 2,
                rec: 2,
                ghost_freq: 0,
                ghost_rec: 1,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        // And assert what's in the sets to be sure of what went where.
        assert!(wr_txn.peek_cache(&1) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&2) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&3) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&4) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&5) == CacheState::Rec);

        // Now we touch 4/5 to make them freq.
        assert!(wr_txn.get(&4) == Some(&4));
        assert!(wr_txn.get(&5) == Some(&5));
        wr_txn.commit();

        // Now we start the fifth txn, and check the stats.
        println!("== 5");
        let mut wr_txn = arc.write();
        println!("stat -> {:?}", wr_txn.peek_stat());
        assert!(
            CStat {
                max: 4,
                cache: 5,
                tlocal: 0,
                freq: 4,
                rec: 0,
                ghost_freq: 0,
                ghost_rec: 1,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        // And assert what's in the sets to be sure of what went where.
        assert!(wr_txn.peek_cache(&1) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&2) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&3) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&4) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&5) == CacheState::Freq);

        // Now, we touch 3, 10, 11, 12, 13. Due to how many additions there are, 3 should be
        // evicted from ghost rec, and all the current freq items should be removed?
        wr_txn.insert(3, 3);
        assert!(wr_txn.get(&3) == Some(&3));
        // When we add 3, we are basically issuing a demand that the rec set should be
        // allowed to grow as we had a potential cache miss here.
        wr_txn.commit();

        // Now we start the sixth txn, and check the stats.
        println!("== 6");
        let mut wr_txn = arc.write();
        println!("stat -> {:?}", wr_txn.peek_stat());
        assert!(
            CStat {
                max: 4,
                cache: 5,
                tlocal: 0,
                freq: 3,
                rec: 1,
                ghost_freq: 1,
                ghost_rec: 0,
                haunted: 0,
                p: 1
            } == wr_txn.peek_stat()
        );
        // And assert what's in the sets to be sure of what went where.
        assert!(wr_txn.peek_cache(&1) == CacheState::GhostFreq);
        assert!(wr_txn.peek_cache(&2) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&3) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&4) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&5) == CacheState::Freq);

        // Right, seventh txn - we show how a cache scan doesn't cause p shifting or evict.
        // tl;dr - attempt to include a bunch in a scan, and it will be ignored as p is low,
        // and any miss on rec won't shift p unless it's in the ghost rec.
        wr_txn.insert(10, 10);
        wr_txn.insert(11, 11);
        wr_txn.insert(12, 12);
        wr_txn.commit();

        println!("== 7");
        let mut wr_txn = arc.write();
        println!("stat -> {:?}", wr_txn.peek_stat());
        assert!(
            CStat {
                max: 4,
                cache: 8,
                tlocal: 0,
                freq: 3,
                rec: 1,
                ghost_freq: 1,
                ghost_rec: 3,
                haunted: 0,
                p: 1
            } == wr_txn.peek_stat()
        );
        assert!(wr_txn.peek_cache(&1) == CacheState::GhostFreq);
        assert!(wr_txn.peek_cache(&2) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&3) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&4) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&5) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&10) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&11) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&12) == CacheState::Rec);

        // Eight txn - now that we had a demand for items before, we re-demand them - this will trigger
        // a shift in p, causing some more to be in the rec cache.
        wr_txn.insert(10, 10);
        wr_txn.insert(11, 11);
        wr_txn.insert(3, 3);
        wr_txn.commit();

        println!("== 8");
        let mut wr_txn = arc.write();
        println!("stat -> {:?}", wr_txn.peek_stat());
        assert!(
            CStat {
                max: 4,
                cache: 8,
                tlocal: 0,
                freq: 0,
                rec: 4,
                ghost_freq: 4,
                ghost_rec: 0,
                haunted: 0,
                p: 4
            } == wr_txn.peek_stat()
        );
        assert!(wr_txn.peek_cache(&1) == CacheState::GhostFreq);
        assert!(wr_txn.peek_cache(&2) == CacheState::GhostFreq);
        assert!(wr_txn.peek_cache(&3) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&4) == CacheState::GhostFreq);
        assert!(wr_txn.peek_cache(&5) == CacheState::GhostFreq);
        assert!(wr_txn.peek_cache(&10) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&11) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&12) == CacheState::Rec);

        // Now lets go back the other way - we want freq items to come back.
        wr_txn.insert(1, 1);
        wr_txn.insert(2, 1);
        wr_txn.insert(4, 1);
        wr_txn.insert(5, 1);
        wr_txn.commit();

        println!("== 9");
        let mut wr_txn = arc.write();
        println!("stat -> {:?}", wr_txn.peek_stat());
        assert!(
            CStat {
                max: 4,
                cache: 8,
                tlocal: 0,
                freq: 4,
                rec: 0,
                ghost_freq: 0,
                ghost_rec: 4,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        assert!(wr_txn.peek_cache(&1) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&2) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&3) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&4) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&5) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&10) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&11) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&12) == CacheState::GhostRec);

        // And done!
        wr_txn.commit();
    }

    #[test]
    fn test_cache_concurrent_basic() {
        // Now we want to check some basic interactions of read and write together.

        // Setup the cache.
        let arc: Arc<usize, usize> = Arc::new(4);
        // start a rd
        {
            let mut rd_txn = arc.read();
            // add items to the rd
            rd_txn.insert(1, 1);
            rd_txn.insert(2, 2);
            rd_txn.insert(3, 3);
            rd_txn.insert(4, 4);
            // end the rd
        }
        arc.try_quiesce();
        // What state is the cache now in?
        println!("== 2");
        let mut wr_txn = arc.write();
        assert!(
            CStat {
                max: 4,
                cache: 4,
                tlocal: 0,
                freq: 0,
                rec: 4,
                ghost_freq: 0,
                ghost_rec: 0,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        assert!(wr_txn.peek_cache(&1) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&2) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&3) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&4) == CacheState::Rec);
        // Magic! Without a single write op we included items!
        // Lets have the read touch two items, and then add two new.
        // This will trigger evict on 1/2
        {
            let mut rd_txn = arc.read();
            // add items to the rd
            assert!(rd_txn.get(&3) == Some(&3));
            assert!(rd_txn.get(&4) == Some(&4));
            rd_txn.insert(5, 5);
            rd_txn.insert(6, 6);
            // end the rd
        }
        // Now commit and check the state.
        wr_txn.commit();
        println!("== 3");
        let mut wr_txn = arc.write();
        assert!(
            CStat {
                max: 4,
                cache: 6,
                tlocal: 0,
                freq: 2,
                rec: 2,
                ghost_freq: 0,
                ghost_rec: 2,
                haunted: 0,
                p: 0
            } == wr_txn.peek_stat()
        );
        assert!(wr_txn.peek_cache(&1) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&2) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&3) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&4) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&5) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&6) == CacheState::Rec);

        // Now trigger hits on 1/2 which will cause a shift in P.
        {
            let mut rd_txn = arc.read();
            // add items to the rd
            rd_txn.insert(1, 1);
            rd_txn.insert(2, 2);
            // end the rd
        }

        wr_txn.commit();
        println!("== 4");
        let mut wr_txn = arc.write();
        assert!(
            CStat {
                max: 4,
                cache: 6,
                tlocal: 0,
                freq: 2,
                rec: 2,
                ghost_freq: 0,
                ghost_rec: 2,
                haunted: 0,
                p: 2
            } == wr_txn.peek_stat()
        );
        assert!(wr_txn.peek_cache(&1) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&2) == CacheState::Rec);
        assert!(wr_txn.peek_cache(&3) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&4) == CacheState::Freq);
        assert!(wr_txn.peek_cache(&5) == CacheState::GhostRec);
        assert!(wr_txn.peek_cache(&6) == CacheState::GhostRec);
    }

    // Test edge cases that are horrifying and could destroy peoples lives
    // and sanity.
    #[test]
    fn test_cache_concurrent_cursed_1() {
        // Case 1 - It's possible for a read transaction to last for a long time,
        // and then have a cache include, which may cause an attempt to include
        // an outdated value into the cache. To handle this the haunted set exists
        // so that all keys and their eviction ids are always tracked for all of time
        // to ensure that we never incorrectly include a value that may have been updated
        // more recently.
        let arc: Arc<usize, usize> = Arc::new(4);

        // Start a wr
        let mut wr_txn = arc.write();
        // Start a rd
        let mut rd_txn = arc.read();
        // Add the value 1,1 via the wr.
        wr_txn.insert(1, 1);

        // assert 1 is not in rd.
        assert!(rd_txn.get(&1) == None);

        // Commit wr
        wr_txn.commit();
        // Even after the commit, it's not in rd.
        assert!(rd_txn.get(&1) == None);
        // begin wr
        let mut wr_txn = arc.write();
        // We now need to flood the cache, to cause ghost rec eviction.
        wr_txn.insert(10, 1);
        wr_txn.insert(11, 1);
        wr_txn.insert(12, 1);
        wr_txn.insert(13, 1);
        wr_txn.insert(14, 1);
        wr_txn.insert(15, 1);
        wr_txn.insert(16, 1);
        wr_txn.insert(17, 1);
        // commit wr
        wr_txn.commit();

        // begin wr
        let mut wr_txn = arc.write();
        // assert that 1 is haunted.
        assert!(wr_txn.peek_cache(&1) == CacheState::Haunted);
        // assert 1 is not in rd.
        assert!(rd_txn.get(&1) == None);
        // now that 1 is hanuted, in rd attempt to insert 1, X
        rd_txn.insert(1, 100);
        // commit wr
        wr_txn.commit();

        // start wr
        let mut wr_txn = arc.write();
        // assert that 1 is still haunted.
        assert!(wr_txn.peek_cache(&1) == CacheState::Haunted);
        // assert that 1, x is in rd.
        assert!(rd_txn.get(&1) == Some(&100));
        // done!
    }
}
