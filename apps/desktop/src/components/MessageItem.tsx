/** Accessible rendering and permission-gated actions for one room message. */
import { useEffect, useRef, useState } from "react";
import type { ChangeEvent, FormEvent } from "react";
import type { RoomMessage, RoomPermissions } from "../domain/conversation";

/** Agent protocol markers promoted into visible status badges. */
type ProtocolMarker = "AGREE" | "PASS";

/** Presentation data derived from one recognized protocol marker. */
interface ProtocolPresentation {
  /** Uppercase marker rendered inside the badge. */
  marker: ProtocolMarker;
  /** Message text with only the structural marker removed. */
  summary: string;
}

/** Props accepted by one timeline message item. */
interface MessageItemProps {
  /** Complete sanitized room message. */
  message: RoomMessage;
  /** Signed-in user identifier used for ownership checks. */
  currentUserId: string;
  /** Server-authoritative room capabilities. */
  permissions: RoomPermissions;
  /** Latest optional presence value for the author. */
  presence?: string;
  /** Persist one owned-message edit, returning true on success. */
  onEdit(messageId: string, content: string): Promise<boolean>;
  /** Persist one authorized deletion, returning true on success. */
  onDelete(messageId: string): Promise<boolean>;
}

/** Props accepted by the sanitized attachment list. */
interface MessageAttachmentsProps {
  /** Attachments returned by the native sanitizer. */
  attachments: RoomMessage["attachments"];
}

/** Marker shape accepted only at the beginning of a logical line. */
const PROTOCOL_MARKER_PATTERN = /(?:^|\n)\s*\[(AGREE|PASS)\](?=\s|$)/i;

/** Produce a concise local timestamp while retaining the exact value in dateTime. */
function formatMessageTime(timestamp: string): string {
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) {
    return timestamp;
  }
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
  }).format(date);
}

/** Return the stable human-facing identity for one sanitized author. */
function messageAuthor(message: RoomMessage): string {
  return message.authorDisplayName ?? message.authorUsername;
}

/** Extract a recognized agent marker without discarding original content. */
function protocolPresentation(message: RoomMessage): ProtocolPresentation | null {
  if (message.messageType !== "agent") {
    return null;
  }
  const match = PROTOCOL_MARKER_PATTERN.exec(message.content);
  if (match === null) {
    return null;
  }
  return {
    marker: match[1].toUpperCase() as ProtocolMarker,
    summary: message.content.replace(PROTOCOL_MARKER_PATTERN, "\n").trim(),
  };
}

/** Render only sanitized server attachment fields. */
function MessageAttachments({ attachments }: MessageAttachmentsProps) {
  if (attachments.length === 0) {
    return null;
  }
  return (
    <ul className="message-item__attachments" aria-label="Message attachments">
      {attachments.map(({ id, filename, url, sizeBytes }) => (
        <li key={id}>
          <a href={url} target="_blank" rel="noreferrer">
            {filename}
          </a>
          {sizeBytes === null ? null : (
            <span className="message-item__attachment-size"> {sizeBytes} B</span>
          )}
        </li>
      ))}
    </ul>
  );
}

/** Render one continuous-timeline message with safe inline mutation controls. */
export function MessageItem({
  message,
  currentUserId,
  permissions,
  presence,
  onEdit,
  onDelete,
}: MessageItemProps) {
  const author = messageAuthor(message);
  const isOwned = message.authorId === currentUserId;
  const canEdit = isOwned && message.messageType === "user";
  const canDelete = isOwned || permissions.manageMessages;
  const protocol = protocolPresentation(message);
  const [editing, setEditing] = useState(false);
  const [editDraft, setEditDraft] = useState(message.content);
  const [saving, setSaving] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const editButtonRef = useRef<HTMLButtonElement>(null);
  const deleteButtonRef = useRef<HTMLButtonElement>(null);
  const confirmDeleteRef = useRef<HTMLButtonElement>(null);
  const editFieldRef = useRef<HTMLTextAreaElement>(null);
  const restoreEditFocusRef = useRef(false);
  const restoreEditFieldFocusRef = useRef(false);
  const restoreDeleteConfirmationFocusRef = useRef(false);
  const structuralType =
    message.messageType === "system" || message.messageType === "stimulus";
  const accessibleLabel =
    message.messageType === "system"
      ? "System message"
      : message.messageType === "stimulus"
        ? "Stimulus message"
        : `Message from ${author}`;

  /** Restore focus to the edit trigger after a completed or canceled edit. */
  useEffect(() => {
    if (!editing && restoreEditFocusRef.current) {
      restoreEditFocusRef.current = false;
      editButtonRef.current?.focus();
    }
  }, [editing]);

  /** Move focus into each newly opened inline action surface. */
  useEffect(() => {
    if (editing) {
      editFieldRef.current?.focus();
    } else if (confirmingDelete) {
      confirmDeleteRef.current?.focus();
    }
  }, [confirmingDelete, editing]);

  /** Restore failed-action focus only after React enables the target control. */
  useEffect(() => {
    if (!saving && restoreEditFieldFocusRef.current) {
      restoreEditFieldFocusRef.current = false;
      editFieldRef.current?.focus();
    }
    if (!deleting && restoreDeleteConfirmationFocusRef.current) {
      restoreDeleteConfirmationFocusRef.current = false;
      confirmDeleteRef.current?.focus();
    }
  }, [deleting, saving]);

  /** Begin an edit from the latest rendered server value. */
  function beginEdit(): void {
    setEditDraft(message.content);
    setConfirmingDelete(false);
    setEditing(true);
  }

  /** Cancel an edit and return focus to its original trigger. */
  function cancelEdit(): void {
    restoreEditFocusRef.current = true;
    setEditDraft(message.content);
    setEditing(false);
  }

  /** Persist a non-empty edit while retaining failed text in place. */
  async function submitEdit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    const content = editDraft.trim();
    if (content.length === 0 || saving) {
      return;
    }
    setSaving(true);
    const saved = await onEdit(message.id, content);
    if (saved) {
      setSaving(false);
      restoreEditFocusRef.current = true;
      setEditing(false);
    } else {
      restoreEditFieldFocusRef.current = true;
      setSaving(false);
    }
  }

  /** Open the explicit deletion confirmation surface. */
  function beginDelete(): void {
    setEditing(false);
    setConfirmingDelete(true);
  }

  /** Close deletion confirmation and restore its trigger focus. */
  function cancelDelete(): void {
    setConfirmingDelete(false);
    queueMicrotask(() => deleteButtonRef.current?.focus());
  }

  /** Delete only after confirmation, retaining the prompt after failures. */
  async function confirmDelete(): Promise<void> {
    if (deleting) {
      return;
    }
    setDeleting(true);
    const deleted = await onDelete(message.id);
    if (deleted) {
      setDeleting(false);
    } else {
      restoreDeleteConfirmationFocusRef.current = true;
      setDeleting(false);
    }
  }

  return (
    <article
      className={`message-item message-item--${message.messageType}`}
      aria-label={accessibleLabel}
      data-message-id={message.id}
      data-message-type={message.messageType}
      role={message.messageType === "system" ? "note" : undefined}
    >
      <header className="message-item__header">
        {message.authorAvatarUrl === null || structuralType ? null : (
          <img
            className="message-item__avatar"
            src={message.authorAvatarUrl}
            alt=""
            width="32"
            height="32"
          />
        )}
        <span className="message-item__identity">
          {message.messageType === "system" ? "System" : author}
        </span>
        {message.messageType === "agent" ? (
          <span className="message-item__author-kind">Agent</span>
        ) : null}
        {message.messageType === "stimulus" ? (
          <span className="message-item__author-kind">Stimulus</span>
        ) : null}
        {presence === undefined || structuralType ? null : (
          <span
            className="message-item__presence"
            aria-label={`${author} is ${presence}`}
          >
            {presence}
          </span>
        )}
        <time dateTime={message.createdAt}>
          {formatMessageTime(message.createdAt)}
        </time>
        {message.editedAt === null ? null : (
          <span className="message-item__edited">Edited</span>
        )}
      </header>

      {editing ? (
        <form className="message-item__edit" onSubmit={(event) => void submitEdit(event)}>
          <label>
            <span>Edit message from {author}</span>
            <textarea
              ref={editFieldRef}
              aria-label={`Edit message from ${author}`}
              value={editDraft}
              disabled={saving}
              onChange={(event: ChangeEvent<HTMLTextAreaElement>) =>
                setEditDraft(event.target.value)
              }
            />
          </label>
          <div className="message-item__edit-actions">
            <button type="submit" disabled={saving || editDraft.trim().length === 0}>
              Save edit
            </button>
            <button type="button" disabled={saving} onClick={cancelEdit}>
              Cancel edit
            </button>
          </div>
        </form>
      ) : (
        <div className="message-item__body">
          {protocol === null ? (
            message.content.length === 0 ? null : <p>{message.content}</p>
          ) : (
            <div className="message-item__protocol">
              <span
                className="message-item__protocol-badge"
                aria-label={`Protocol marker ${protocol.marker}`}
              >
                {protocol.marker}
              </span>
              {protocol.summary.length === 0 ? null : <p>{protocol.summary}</p>}
              <details>
                <summary>Inspect original message</summary>
                <p>{message.content}</p>
              </details>
            </div>
          )}
          <MessageAttachments attachments={message.attachments} />
        </div>
      )}

      {editing || confirmingDelete || (!canEdit && !canDelete) ? null : (
        <div className="message-item__actions">
          {canEdit ? (
            <button
              ref={editButtonRef}
              type="button"
              aria-label={`Edit message from ${author}`}
              onClick={beginEdit}
            >
              Edit
            </button>
          ) : null}
          {canDelete ? (
            <button
              ref={deleteButtonRef}
              type="button"
              aria-label={`Delete message from ${author}`}
              onClick={beginDelete}
            >
              Delete
            </button>
          ) : null}
        </div>
      )}

      {confirmingDelete ? (
        <div
          className="message-item__delete-confirmation"
          role="alertdialog"
          aria-label="Delete message confirmation"
          aria-describedby={`delete-message-${message.id}`}
        >
          <p id={`delete-message-${message.id}`}>
            Delete this message permanently?
          </p>
          <button
            ref={confirmDeleteRef}
            type="button"
            disabled={deleting}
            onClick={() => void confirmDelete()}
          >
            Confirm delete
          </button>
          <button type="button" disabled={deleting} onClick={cancelDelete}>
            Cancel deletion
          </button>
        </div>
      ) : null}
    </article>
  );
}
