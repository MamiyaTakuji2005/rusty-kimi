Undo the last file writes made by WriteFile and StrReplaceFile, newest first.

- A modified file goes back to its previous content; a file the write created is deleted.
- Shell commands and subagent writes are not tracked and cannot be undone here.
- Only recent writes are kept, very large files are recorded but not restorable, and the history is discarded when the session ends.
