
extern crate concread;
use concread::cowcell::CowCell;
use std::ops::Deref;
// use crossbeam_epoch::*;
use std::rc::Rc;
use std::mem::forget;


struct StrRef<'a> {
    r: &'a str,
}

impl<'a> Drop for StrRef<'a> {
    fn drop(&mut self) {
        println!("{}", self.r);
    }
}

impl<'a> Clone for StrRef<'a> {
    fn clone(&self) -> StrRef<'a> {
        StrRef { r: self.r }
    }
}

struct ChangesItselfString {
    pub s: Rc<String>,
}

impl Drop for ChangesItselfString {
    fn drop(&mut self) {
        Rc::get_mut(&mut self.s).unwrap().as_mut_str().make_ascii_uppercase();
        // Keep object alive.
        forget(self.s.clone());
    }
}

fn main() {
    {
        let s = ChangesItselfString { s: Rc::new(String::from("lowercase_string")) };
        let f = StrRef { r: s.s.deref() };
        let cell = CowCell::new(f);
        drop(cell);
    }
    println!("ChangesItselfString is gone!");

    // StrRef drop() references possibly freed memory! I made it view a leaked Rc so as to
    // not get a segfault, but in general this can be made to do all kinds of wrong things.
    // pin().flush();
    // pin().flush();
}

