/** Path-free pending attachment controls for the room composer. */
import type {
  ConversationUploadProgress,
  PendingRoomAttachment,
} from "../domain/conversation";

/** Props accepted by the pending attachment tray. */
interface AttachmentTrayProps {
  /** Sanitized uploads staged for the next room message. */
  attachments: readonly PendingRoomAttachment[];
  /** Latest native progress keyed by the same opaque upload identifier. */
  progressByUploadId: ReadonlyMap<string, ConversationUploadProgress>;
  /** Whether attachment mutations are temporarily unavailable. */
  disabled: boolean;
  /** Remove one staged upload before its message is sent. */
  onRemove(uploadId: string): void;
}

/** Format a byte count without introducing locale-dependent precision. */
function formatBytes(bytes: number): string {
  if (bytes < 1_024) {
    return `${bytes} B`;
  }
  return `${(bytes / 1_024).toFixed(1)} KB`;
}

/** Render staged uploads, bounded progress, and pre-send removal controls. */
export function AttachmentTray({
  attachments,
  progressByUploadId,
  disabled,
  onRemove,
}: AttachmentTrayProps) {
  if (attachments.length === 0) {
    return null;
  }

  return (
    <aside className="attachment-tray" aria-label="Pending attachments">
      <ul className="attachment-tray__list">
        {attachments.map(({ uploadId, filename, sizeBytes }) => {
          const progress = progressByUploadId.get(uploadId);
          const bytesSent = Math.min(
            Math.max(progress?.bytesSent ?? sizeBytes, 0),
            Math.max(progress?.totalBytes ?? sizeBytes, 0),
          );
          const totalBytes = Math.max(progress?.totalBytes ?? sizeBytes, 1);

          return (
            <li className="attachment-tray__item" key={uploadId}>
              <div className="attachment-tray__metadata">
                <span className="attachment-tray__filename">{filename}</span>
                <span className="attachment-tray__size">
                  {formatBytes(bytesSent)} of {formatBytes(totalBytes)}
                </span>
              </div>
              <progress
                className="attachment-tray__progress"
                aria-label={`Upload progress for ${filename}`}
                max={totalBytes}
                value={bytesSent}
              />
              <button
                className="attachment-tray__remove"
                type="button"
                disabled={disabled}
                aria-label={`Remove ${filename}`}
                onClick={() => onRemove(uploadId)}
              >
                Remove
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
