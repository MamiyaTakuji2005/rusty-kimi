Fold recent context into a single message by rewinding to an earlier checkpoint.

Checkpoints appear in the context as `user` messages reading `CHECKPOINT {checkpoint_id}` inside `<system>` tags. Sending a D-Mail rewinds the context to the checkpoint you name and appends your message in place of everything after it. Nothing else is reverted — files and any other external state stay exactly as they are.

- Use it after work that cost a lot of context but produced a small result: a large file read, a broad search, a long debugging detour.
- Write the message to your past self: what you did, what you learned, and what not to repeat. Anything you leave out is gone.
- The message is for you, not the user — do not explain the D-Mail to them.
