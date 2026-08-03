/** Accessible message composer with typing throttling and staged attachments. */
import { useRef, useState } from "react";
import type { ChangeEvent, FormEvent, KeyboardEvent } from "react";
import type {
  ConversationUploadProgress,
  PendingRoomAttachment,
} from "../domain/conversation";
import { AttachmentTray } from "./AttachmentTray";

/** Minimum interval between native typing notifications. */
const TYPING_THROTTLE_MS = 3_000;

/** Props accepted by the room message composer. */
interface RoomComposerProps {
  /** Sanitized uploads staged for the next message. */
  attachments: readonly PendingRoomAttachment[];
  /** Latest native progress keyed by opaque upload identifier. */
  progressByUploadId: ReadonlyMap<string, ConversationUploadProgress>;
  /** Whether the signed-in user may send room messages. */
  canSend: boolean;
  /** Whether the signed-in user may stage files. */
  canAttach: boolean;
  /** Whether another room mutation is currently running. */
  busy: boolean;
  /** Ask native code to select and stage bounded attachments. */
  onSelectAttachments(): Promise<void>;
  /** Remove one staged upload before sending. */
  onRemoveAttachment(uploadId: string): void;
  /** Send text and staged upload identifiers, returning true on success. */
  onSend(content: string, uploadIds: string[]): Promise<boolean>;
  /** Emit one coalesced room typing notification. */
  onTyping(): void;
}

/** Render the primary room composer while retaining drafts after failures. */
export function RoomComposer({
  attachments,
  progressByUploadId,
  canSend,
  canAttach,
  busy,
  onSelectAttachments,
  onRemoveAttachment,
  onSend,
  onTyping,
}: RoomComposerProps) {
  const [draft, setDraft] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const lastTypingAtRef = useRef(Number.NEGATIVE_INFINITY);
  const draftRevisionRef = useRef(0);
  const sendInFlightRef = useRef(false);
  const uploadIds = attachments.map((attachment) => attachment.uploadId);
  const hasSendableContent = draft.trim().length > 0 || uploadIds.length > 0;

  /** Submit one text or attachment-only message without clearing failed input. */
  async function submitMessage(): Promise<void> {
    if (!canSend || busy || !hasSendableContent || sendInFlightRef.current) {
      return;
    }
    const submittedRevision = draftRevisionRef.current;
    sendInFlightRef.current = true;
    try {
      const sent = await onSend(draft.trim(), uploadIds);
      if (sent) {
        setDraft((currentDraft) =>
          draftRevisionRef.current === submittedRevision ? "" : currentDraft,
        );
      }
    } finally {
      sendInFlightRef.current = false;
    }
    textareaRef.current?.focus();
  }

  /** Keep the form submission path aligned with the Enter-key behavior. */
  function handleSubmit(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    void submitMessage();
  }

  /** Send on Enter while preserving Shift+Enter as a multiline edit. */
  function handleKeyDown(event: KeyboardEvent<HTMLTextAreaElement>): void {
    if (
      event.key !== "Enter" ||
      event.shiftKey ||
      event.nativeEvent.isComposing
    ) {
      return;
    }
    event.preventDefault();
    void submitMessage();
  }

  /** Update the local draft and emit at most one typing event per throttle window. */
  function handleChange(event: ChangeEvent<HTMLTextAreaElement>): void {
    const nextDraft = event.target.value;
    draftRevisionRef.current += 1;
    setDraft(nextDraft);
    if (!canSend || nextDraft.length === 0) {
      return;
    }
    const now = Date.now();
    if (now - lastTypingAtRef.current >= TYPING_THROTTLE_MS) {
      lastTypingAtRef.current = now;
      onTyping();
    }
  }

  return (
    <form
      className="room-composer"
      aria-label="Room message composer"
      onSubmit={handleSubmit}
    >
      <AttachmentTray
        attachments={attachments}
        progressByUploadId={progressByUploadId}
        disabled={busy}
        onRemove={onRemoveAttachment}
      />
      <label className="room-composer__field">
        <span className="room-composer__label">Message Rift room</span>
        <textarea
          ref={textareaRef}
          className="room-composer__textarea"
          aria-label="Message Rift room"
          value={draft}
          disabled={!canSend}
          rows={3}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
        />
      </label>
      <p className="room-composer__hint">
        Enter sends. Shift+Enter starts a new line.
      </p>
      <div className="room-composer__actions">
        <button
          className="room-composer__attach"
          type="button"
          disabled={!canAttach || busy}
          onClick={() => void onSelectAttachments()}
        >
          Add attachments
        </button>
        <button
          className="room-composer__send"
          type="submit"
          disabled={!canSend || busy || !hasSendableContent}
        >
          Send message
        </button>
      </div>
    </form>
  );
}
