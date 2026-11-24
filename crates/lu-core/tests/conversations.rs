use lu_core::{
    ansf_live_chat::{LiveEvent, LiveKind},
    chat::new_id,
    conversations::{title_from, ConversationLibrary, LibraryError},
};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-conv-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn say(lib: &ConversationLibrary, chat: &str, role: LiveKind, text: &str) {
    let book = lib.book(chat).unwrap();
    let user = role == LiveKind::User;
    book.append_host_chat_event(chat, &LiveEvent { event_id: new_id('e'), kind: role, text: text.into(), user_turn_id: user.then(|| new_id('g')), legacy_turn_id: None, generation_id: None })
        .unwrap();
    lib.note_message(chat, if user { "user" } else { "assistant" }, text).unwrap();
}

#[test]
fn conversations_folders_moves_and_titles() {
    let lib = ConversationLibrary::open(&base("basic")).unwrap();
    let a = lib.create(None).unwrap();
    let b = lib.create(None).unwrap();
    say(&lib, &a, LiveKind::User, "Plan the Windows installer for Lu\nwith NSIS");
    say(&lib, &a, LiveKind::AssistantFinal, "Here is a plan using makensis.");
    say(&lib, &b, LiveKind::User, "What is a good name for a cat?");
    let (folders, chats) = lib.list().unwrap();
    assert!(folders.is_empty());
    assert_eq!(chats[0].id, b, "most recent first");
    assert_eq!(lib.get(&a).unwrap().title, "Plan the Windows installer for Lu");
    assert_eq!(lib.messages(&a).unwrap(), [("user".to_owned(), "Plan the Windows installer for Lu\nwith NSIS".to_owned()), ("assistant".to_owned(), "Here is a plan using makensis.".to_owned())]);

    // A user's own title is kept.
    lib.rename(&b, "Cats").unwrap();
    say(&lib, &b, LiveKind::User, "Another question");
    assert_eq!(lib.get(&b).unwrap().title, "Cats");

    // Folders and moving (drag and drop).
    let work = lib.create_folder("Work").unwrap();
    lib.move_to(&a, Some(work)).unwrap();
    assert_eq!(lib.get(&a).unwrap().folder, Some(work));
    assert_eq!(lib.move_to(&a, Some(9999)), Err(LibraryError::NotFound));
    lib.rename_folder(work, "Lu work").unwrap();
    assert_eq!(lib.list().unwrap().0[0].name, "Lu work");
    lib.move_to(&a, None).unwrap();
    assert_eq!(lib.get(&a).unwrap().folder, None);
    assert!(lib.create_folder("  ").is_err());

    // Deleting a folder keeps its conversations, unless asked otherwise.
    lib.move_to(&a, Some(work)).unwrap();
    lib.delete_folder(work, false).unwrap();
    assert_eq!(lib.get(&a).unwrap().folder, None);
    let old = lib.create_folder("Old").unwrap();
    lib.move_to(&b, Some(old)).unwrap();
    lib.delete_folder(old, true).unwrap();
    assert_eq!(lib.get(&b), Err(LibraryError::NotFound));
    assert_eq!(lib.trashed().unwrap().len(), 1);
    assert_eq!(title_from(&"x".repeat(80)).chars().count(), 61);
}

#[test]
fn search_finds_text_across_conversations_and_delete_removes_it() {
    let dir = base("search");
    let lib = ConversationLibrary::open(&dir).unwrap();
    let a = lib.create(None).unwrap();
    let b = lib.create(None).unwrap();
    say(&lib, &a, LiveKind::User, "Where is makensis installed?");
    say(&lib, &a, LiveKind::AssistantFinal, "It is in C:/tools/nsis.");
    say(&lib, &b, LiveKind::User, "Tell me about the R9700 GPU");
    let hits = lib.search("makensis", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chat, a);
    assert!(hits[0].snippet.contains("[makensis]"), "{}", hits[0].snippet);
    assert_eq!(lib.search("R9700", 10).unwrap()[0].chat, b);
    assert!(lib.search("nothing-like-this", 10).unwrap().is_empty());
    // Quotes and operators in a query are taken literally.
    assert!(lib.search("\"makensis\" OR NEAR(", 10).is_ok());

    // Delete: gone from list and search, book moved to the trash; restore brings it back.
    let trash = lib.delete(&a).unwrap();
    assert!(lib.search("makensis", 10).unwrap().is_empty());
    assert!(!dir.join(format!("chats/{a}.sqlite")).exists());
    assert!(dir.join(format!("trash/{trash}/{a}.sqlite")).exists());
    let back = lib.restore(&trash).unwrap();
    assert_eq!(back, a);
    assert_eq!(lib.search("makensis", 10).unwrap()[0].chat, a);
    assert_eq!(lib.messages(&a).unwrap().len(), 2);
    // Permanent deletion empties it for good.
    let t2 = lib.delete(&b).unwrap();
    lib.empty_trash_item(&t2).unwrap();
    assert!(!dir.join(format!("trash/{t2}")).exists());
    assert!(lib.trashed().unwrap().is_empty());
    assert!(lib.empty_trash_item("../library").is_err());
}
