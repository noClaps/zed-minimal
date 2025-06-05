use cocoa::base::id;
use cocoa::foundation::NSRange;
use objc::{class, msg_send, sel, sel_impl};

/// The `cocoa` crate does not define NSAttributedString (and related Cocoa classes),
/// which are needed for copying rich text (that is, text intermingled with images)
/// to the clipboard. This adds access to those APIs.
#[allow(non_snake_case)]
pub trait NSAttributedString: Sized {
    unsafe fn alloc(_: Self) -> id {
        msg_send![class!(NSAttributedString), alloc]
    }

    unsafe fn init_attributed_string(self, string: id) -> id;
    unsafe fn appendAttributedString_(self, attr_string: id);
    unsafe fn RTFDFromRange_documentAttributes_(self, range: NSRange, attrs: id) -> id;
    unsafe fn RTFFromRange_documentAttributes_(self, range: NSRange, attrs: id) -> id;
    unsafe fn string(self) -> id;
}

impl NSAttributedString for id {
    unsafe fn init_attributed_string(self, string: id) -> id {
        msg_send![self, initWithString: string]
    }

    unsafe fn appendAttributedString_(self, attr_string: id) {
        let _: () = msg_send![self, appendAttributedString: attr_string];
    }

    unsafe fn RTFDFromRange_documentAttributes_(self, range: NSRange, attrs: id) -> id {
        msg_send![self, RTFDFromRange: range documentAttributes: attrs]
    }

    unsafe fn RTFFromRange_documentAttributes_(self, range: NSRange, attrs: id) -> id {
        msg_send![self, RTFFromRange: range documentAttributes: attrs]
    }

    unsafe fn string(self) -> id {
        msg_send![self, string]
    }
}

pub trait NSMutableAttributedString: NSAttributedString {
    unsafe fn alloc(_: Self) -> id {
        msg_send![class!(NSMutableAttributedString), alloc]
    }
}

impl NSMutableAttributedString for id {}
