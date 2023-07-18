use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::storagev2::{
    disk::Disk,
    page::{Page, PageID},
    replacer::LrukReplacer,
};

pub enum PageIndex {
    Write,
    Read(usize),
}

pub const DEFAULT_PAGE_SIZE: usize = 4 * 1024;
pub const DEFAULT_READ_SIZE: usize = 8;

pub struct PageManager<const PAGE_SIZE: usize, const READ_SIZE: usize> {
    disk: Disk,
    page_table: HashMap<PageID, PageIndex>, // Map page ids to index
    current: Arc<RwLock<Page<PAGE_SIZE>>>,
    read: *mut [Option<RwLock<Page<PAGE_SIZE>>>; READ_SIZE], // Read only pages
    free: Vec<usize>,
    next_id: AtomicUsize,
    replacer: LrukReplacer,
}

impl<const PAGE_SIZE: usize, const READ_SIZE: usize> PageManager<PAGE_SIZE, READ_SIZE> {
    pub fn new(disk: Disk) -> Self {
        // TODO: bootstrap process could give us the write page and next_id
        let current_page_id = 0;
        let current = Arc::new(RwLock::new(Page::<PAGE_SIZE>::new(current_page_id)));
        let page_table = HashMap::from([(current_page_id, PageIndex::Write)]);
        let read = Box::into_raw(Box::new(std::array::from_fn(|_| None)));
        let next_id = AtomicUsize::new(1);
        let free = (0..READ_SIZE).rev().collect();
        let replacer = LrukReplacer::new(2);

        Self {
            disk,
            page_table,
            current,
            read,
            free,
            next_id,
            replacer,
        }
    }

    pub fn inc_id(&self) -> PageID {
        self.next_id.fetch_add(1, Ordering::Relaxed) as u32
    }

    pub async fn replace_page(&mut self) -> io::Result<()> {
        let mut page_w = self.current.write().await;
        self.disk.write_page(&page_w)?;

        let old_id = page_w.id;
        if let None = self.page_table.remove(&old_id) {
            eprintln!("No write page while replacing write page");
        }

        let id = self.inc_id();
        *page_w = Page::new(id);
        self.page_table.insert(id, PageIndex::Write);

        Ok(())
    }

    pub async fn new_page<'a>(&mut self) -> Option<RwLockReadGuard<'a, Page<PAGE_SIZE>>> {
        let i = if let Some(i) = self.free.pop() {
            i
        } else {
            let Some(i) = self.replacer.evict() else { return None };
            // self.disk.write_page(&page);

            i
        };
        self.replacer.record_access(i);

        let page_id = self.inc_id();
        let page = Page::<PAGE_SIZE>::new(page_id);
        page.pin();
        self.disk.write_page(&page).expect("Couldn't write page");
        self.page_table.insert(page_id, PageIndex::Read(i));

        let page_r = unsafe {
            (*self.read)[i].replace(RwLock::new(page));
            (*self.read)[i].as_ref().unwrap().read().await
        };

        Some(page_r)
    }

    pub async fn fetch_page(
        &mut self,
        page_id: PageID,
    ) -> Option<RwLockReadGuard<'_, Page<PAGE_SIZE>>> {
        if let Some(i) = self.page_table.get(&page_id) {
            return match i {
                PageIndex::Write => {
                    let page = self.current.as_ref().read().await;
                    Some(page)
                }
                PageIndex::Read(i) => unsafe {
                    assert!(*i < READ_SIZE);
                    self.replacer.record_access(*i);

                    let page = (*self.read)[*i]
                        .as_ref()
                        .expect("Invalid page index in table")
                        .read()
                        .await;
                    page.pin();
                    Some(page)
                },
            };
        };

        let i = if let Some(i) = self.free.pop() {
            i
        } else {
            let Some(i) = self.replacer.evict() else { return None };
            self.replacer.record_access(i);
            // self.disk.write_page(&page);

            i
        };

        assert!(i < READ_SIZE);
        unsafe {
            let mut _lock_w;
            if let Some(page) = &(*self.read)[i] {
                self.page_table.remove(&page_id);
                _lock_w = page.write();
            }

            let page = self
                .disk
                .read_page::<PAGE_SIZE>(page_id)
                .expect("Couldn't read page");

            // If we hold a write lock to the page we're going to replace, this should be safe
            (*self.read)[i].replace(RwLock::new(page));

            let page = (*self.read)[i].as_ref().unwrap().read().await;
            page.pin();
            Some(page)
        }
    }

    pub async fn unpin_page(&mut self, page_id: PageID) {
        let Some(i) = self.page_table.get(&page_id) else { return };

        let i = match i {
            PageIndex::Read(i) => i,
            _ => todo!(),
        };

        let page = unsafe { (*self.read)[*i].as_ref().unwrap().read().await };

        if page.pins.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.replacer.set_evictable(*i, true);
        }
    }

    pub async fn get_current(&self) -> RwLockWriteGuard<Page<PAGE_SIZE>> {
        self.current.write().await
    }
}

#[cfg(test)]
mod test {
    use std::io;

    use crate::storagev2::{
        disk::Disk,
        log::{Entry, EntryType},
        page_manager::{PageManager, DEFAULT_PAGE_SIZE, DEFAULT_READ_SIZE},
        test::CleanUp,
    };

    #[tokio::test]
    async fn test_page_manager() -> io::Result<()> {
        const DB_FILE: &str = "./test_page_manager.db";
        let _cu = CleanUp::file(DB_FILE);
        let disk = Disk::new(DB_FILE).await?;

        let mut m = PageManager::<DEFAULT_PAGE_SIZE, DEFAULT_READ_SIZE>::new(disk);

        let mut page_w = m.get_current().await;

        let entry_a = Entry::new(b"test_keya", b"test_valuea", EntryType::Put);
        let entry_b = Entry::new(b"test_keyb", b"test_valueb", EntryType::Put);
        let offset_a = page_w.write_entry(&entry_a).expect("should not be full");
        let offset_b = page_w.write_entry(&entry_b).expect("should not be full");

        assert!(offset_a == 0);
        assert!(offset_b == entry_a.len());
        drop(page_w);

        let page_r = m.fetch_page(0).await.expect("should fetch current page");

        let got_a = page_r.read_entry(0);
        let got_b = page_r.read_entry(entry_a.len());

        assert!(
            entry_a == got_a,
            "\nExpected: {:?}\nGot: {:?}\n",
            entry_a,
            got_a
        );
        assert!(
            entry_b == got_b,
            "\nExpected: {:?}\nGot: {:?}\n",
            entry_b,
            got_b
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_replacer() -> io::Result<()> {
        const DB_FILE: &str = "./test_replacer.db";
        let _cu = CleanUp::file(DB_FILE);
        let disk = Disk::new(DB_FILE).await?;

        let mut m = PageManager::<DEFAULT_PAGE_SIZE, 3>::new(disk);

        let a = m.new_page().await.expect("should have space for page 1"); // ts = 0
        let b = m.new_page().await.expect("should have space for page 2"); // ts = 1
        let c = m.new_page().await.expect("should have space for page 3"); // ts = 2

        let _ = m.fetch_page(1); // ts = 3
        let _ = m.fetch_page(2); // ts = 4
        let _ = m.fetch_page(1); // ts = 5

        let _ = m.fetch_page(1); // ts = 6
        let _ = m.fetch_page(2); // ts = 7
        let _ = m.fetch_page(1); // ts = 8
        let _ = m.fetch_page(2); // ts = 9

        let _ = m.fetch_page(3); // ts = 10 - Least accessed, should get evicted

        m.unpin_page(1).await;
        m.unpin_page(2).await;
        m.unpin_page(3).await;

        let new_page = m.new_page().await.expect("a page should have been evicted");
        assert!(new_page.id == 4);

        let pages = unsafe { &(*m.read) };
        let expected_ids = vec![1, 2, 4];
        let mut actual_ids = Vec::new();
        for page in pages.iter() {
            if let Some(page) = page {
                actual_ids.push(page.read().await.id)
            }
        }

        assert!(
            expected_ids == actual_ids,
            "\nExpected: {:?}\nGot: {:?}\n",
            expected_ids,
            actual_ids
        );

        Ok(())
    }
}