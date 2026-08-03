/** Generation-scoped Rift room conversation lifecycle and interaction container. */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  advanceConversationCommand,
  applyConversationDeleteResult,
  applyConversationEvent,
  applyConversationMessageResult,
  applyConversationOlderPageResult,
  expireConversationTyping,
  markConversationReadThrough,
  replaceConversationSnapshot,
  setConversationLiveEdge,
} from "../domain/conversation";
import type {
  BufferedConversationEvent,
  ConversationState,
  PendingRoomAttachment,
  RoomConversationEventEnvelope,
  RoomConnectionStatus,
} from "../domain/conversation";
import { HenosisClientError, normalizeClientError } from "../services/henosisClient";
import type { HenosisClient } from "../services/henosisClient";
import { MessageTimeline } from "./MessageTimeline";
import { RoomComposer } from "./RoomComposer";

/** Props accepted by the complete room conversation workspace. */
export interface RoomConversationProps {
  /** Runtime adapter that keeps Rift credentials outside React. */
  client: HenosisClient;
  /** Exact room selected by the surrounding workspace. */
  roomId: string;
}

/** Create a random URL-safe one-use capability for one room generation. */
function createRoomStreamId(): string {
  if (typeof globalThis.crypto.randomUUID === "function") {
    return globalThis.crypto.randomUUID().replaceAll("-", "");
  }
  const bytes = globalThis.crypto.getRandomValues(new Uint8Array(18));
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** Convert a transport discriminator into concise live-region copy. */
function connectionStatusLabel(status: RoomConnectionStatus): string {
  return `${status.charAt(0).toUpperCase()}${status.slice(1)}`;
}

/** Copy only the path-free attachment fields admitted by the React contract. */
function sanitizePendingAttachment(
  attachment: PendingRoomAttachment,
): PendingRoomAttachment {
  return {
    uploadId: attachment.uploadId,
    filename: attachment.filename,
    contentType: attachment.contentType,
    sizeBytes: attachment.sizeBytes,
  };
}

/** Reject a command result that does not carry the active one-use generation. */
function assertMatchingCommandStream(
  expectedStreamId: string,
  resultStreamId: string,
): void {
  if (resultStreamId !== expectedStreamId) {
    throw new HenosisClientError(
      "protocol",
      "Rift returned a room command that Henosis could not verify.",
    );
  }
}

/** Render and own one exact native room generation. */
export function RoomConversation({ client, roomId }: RoomConversationProps) {
  const [attempt, setAttempt] = useState(0);
  const [conversation, setConversation] = useState<ConversationState | null>(null);
  const [openingError, setOpeningError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [pendingAttachments, setPendingAttachments] = useState<
    PendingRoomAttachment[]
  >([]);
  const conversationRef = useRef<ConversationState | null>(null);
  const readInFlightRef = useRef<string | null>(null);

  /** Project one reducer transition against the latest committed React state. */
  const projectConversation = useCallback(
    (project: (current: ConversationState) => ConversationState): void => {
      setConversation((current) => {
        if (current === null) {
          return current;
        }
        const next = project(current);
        conversationRef.current = next;
        return next;
      });
    },
    [],
  );

  /** Open, subscribe, reconcile, and close one exact room generation. */
  useEffect(() => {
    const streamId = createRoomStreamId();
    const bufferedEvents: BufferedConversationEvent[] = [];
    let active = true;
    let ready = false;
    let unlisten: (() => void) | null = null;
    let openStarted = false;
    let openSettled = false;
    let closeRequested = false;
    let closeStarted = false;

    conversationRef.current = null;
    readInFlightRef.current = null;
    setConversation(null);
    setOpeningError(null);
    setActionError(null);
    setBusyAction(null);
    setPendingAttachments([]);

    /** Close this generation at most once and ignore stale-close rejection. */
    function closeGeneration(): void {
      if (closeStarted) {
        return;
      }
      closeStarted = true;
      void client.closeRoom(roomId, streamId).catch(() => undefined);
    }

    /** Stop event delivery and release this exact generation at most once. */
    function releaseGeneration(): void {
      active = false;
      ready = false;
      bufferedEvents.length = 0;
      unlisten?.();
      unlisten = null;
      conversationRef.current = null;
      closeRequested = true;
      if (!openStarted || openSettled) {
        closeGeneration();
      }
    }

    /** Buffer pre-snapshot events and project later events through the reducer. */
    function receiveEvent(envelope: RoomConversationEventEnvelope): void {
      if (!active || envelope.streamId !== streamId) {
        return;
      }
      const receivedAt = Date.now();
      if (!ready) {
        bufferedEvents.push({ envelope, receivedAt });
        return;
      }
      projectConversation((current) =>
        applyConversationEvent(current, envelope, receivedAt),
      );
    }

    void (async () => {
      try {
        const removeListener = await client.subscribeRoomEvents(receiveEvent);
        if (!active) {
          removeListener();
          return;
        }
        unlisten = removeListener;
        openStarted = true;
        let openingSnapshot;
        try {
          openingSnapshot = await client.openRoom(roomId, streamId);
        } finally {
          openSettled = true;
          if (closeRequested) {
            closeGeneration();
          }
        }
        if (!active) {
          return;
        }
        if (
          openingSnapshot.roomId !== roomId ||
          openingSnapshot.streamId !== streamId
        ) {
          throw new HenosisClientError(
            "protocol",
            "Rift returned a room generation that Henosis could not verify.",
          );
        }
        const next = replaceConversationSnapshot(
          null,
          openingSnapshot,
          bufferedEvents,
        );
        bufferedEvents.length = 0;
        ready = true;
        conversationRef.current = next;
        setConversation(next);
      } catch (error) {
        if (active) {
          const message = normalizeClientError(error).message;
          releaseGeneration();
          setOpeningError(message);
        }
      }
    })();

    return () => {
      releaseGeneration();
    };
  }, [attempt, client, projectConversation, roomId]);

  /** Expire typing indicators at their next deterministic deadline. */
  useEffect(() => {
    if (conversation === null || conversation.typingByUserId.size === 0) {
      return;
    }
    const nextExpiry = Math.min(
      ...[...conversation.typingByUserId.values()].map(
        (indicator) => indicator.expiresAt,
      ),
    );
    const timeout = window.setTimeout(() => {
      projectConversation((current) => expireConversationTyping(current, Date.now()));
    }, Math.max(nextExpiry - Date.now(), 0));
    return () => window.clearTimeout(timeout);
  }, [conversation, projectConversation]);

  /** Surface one safe action failure only while its generation remains current. */
  const reportActionError = useCallback((streamId: string, error: unknown): void => {
    if (conversationRef.current?.streamId === streamId) {
      setActionError(normalizeClientError(error).message);
    }
  }, []);

  /** Load and reducer-project one explicit older page. */
  const handleLoadOlder = useCallback(async (): Promise<number | null> => {
    const current = conversationRef.current;
    const oldestMessage = current?.messages[0];
    if (current === null || oldestMessage === undefined || !current.hasOlder) {
      return null;
    }
    setBusyAction("history");
    setActionError(null);
    try {
      const result = await client.loadOlderMessages(
        current.roomId,
        current.streamId,
        oldestMessage.id,
      );
      if (conversationRef.current?.streamId !== current.streamId) {
        return null;
      }
      assertMatchingCommandStream(current.streamId, result.streamId);
      projectConversation((state) =>
        applyConversationOlderPageResult(state, result),
      );
      return result.sequence;
    } catch (error) {
      reportActionError(current.streamId, error);
      return null;
    } finally {
      if (conversationRef.current?.streamId === current.streamId) {
        setBusyAction(null);
      }
    }
  }, [client, projectConversation, reportActionError]);

  /** Send one text or attachment-only message through the active generation. */
  const handleSend = useCallback(
    async (content: string, uploadIds: string[]): Promise<boolean> => {
      const current = conversationRef.current;
      if (current === null) {
        return false;
      }
      setBusyAction("send");
      setActionError(null);
      try {
        const result = await client.sendRoomMessage(
          current.roomId,
          current.streamId,
          content,
          uploadIds,
        );
        if (conversationRef.current?.streamId !== current.streamId) {
          return false;
        }
        assertMatchingCommandStream(current.streamId, result.streamId);
        projectConversation((state) =>
          applyConversationMessageResult(state, result),
        );
        setPendingAttachments([]);
        return true;
      } catch (error) {
        reportActionError(current.streamId, error);
        return false;
      } finally {
        if (conversationRef.current?.streamId === current.streamId) {
          setBusyAction(null);
        }
      }
    },
    [client, projectConversation, reportActionError],
  );

  /** Persist one owned-message edit through the ordered command stream. */
  const handleEdit = useCallback(
    async (messageId: string, content: string): Promise<boolean> => {
      const current = conversationRef.current;
      if (current === null) {
        return false;
      }
      setBusyAction("edit");
      setActionError(null);
      try {
        const result = await client.editRoomMessage(
          current.roomId,
          current.streamId,
          messageId,
          content,
        );
        if (conversationRef.current?.streamId !== current.streamId) {
          return false;
        }
        assertMatchingCommandStream(current.streamId, result.streamId);
        projectConversation((state) =>
          applyConversationMessageResult(state, result),
        );
        return true;
      } catch (error) {
        reportActionError(current.streamId, error);
        return false;
      } finally {
        if (conversationRef.current?.streamId === current.streamId) {
          setBusyAction(null);
        }
      }
    },
    [client, projectConversation, reportActionError],
  );

  /** Persist one confirmed deletion through the ordered command stream. */
  const handleDelete = useCallback(
    async (messageId: string): Promise<boolean> => {
      const current = conversationRef.current;
      if (current === null) {
        return false;
      }
      setBusyAction("delete");
      setActionError(null);
      try {
        const result = await client.deleteRoomMessage(
          current.roomId,
          current.streamId,
          messageId,
        );
        if (conversationRef.current?.streamId !== current.streamId) {
          return false;
        }
        assertMatchingCommandStream(current.streamId, result.streamId);
        projectConversation((state) =>
          applyConversationDeleteResult(state, result),
        );
        return true;
      } catch (error) {
        reportActionError(current.streamId, error);
        return false;
      } finally {
        if (conversationRef.current?.streamId === current.streamId) {
          setBusyAction(null);
        }
      }
    },
    [client, projectConversation, reportActionError],
  );

  /** Ask native code to stage files and retain only approved metadata fields. */
  const handleSelectAttachments = useCallback(async (): Promise<void> => {
    const current = conversationRef.current;
    if (current === null || !current.permissions.attachFiles) {
      return;
    }
    setBusyAction("attachments");
    setActionError(null);
    try {
      const result = await client.selectAndUploadRoomAttachments(
        current.roomId,
        current.streamId,
      );
      if (conversationRef.current?.streamId !== current.streamId) {
        return;
      }
      assertMatchingCommandStream(current.streamId, result.streamId);
      projectConversation((state) => advanceConversationCommand(state, result));
      const sanitized = result.value.map(sanitizePendingAttachment);
      setPendingAttachments((existing) => {
        const byUploadId = new Map(
          existing.map((attachment) => [attachment.uploadId, attachment]),
        );
        for (const attachment of sanitized) {
          byUploadId.set(attachment.uploadId, attachment);
        }
        return [...byUploadId.values()];
      });
    } catch (error) {
      reportActionError(current.streamId, error);
    } finally {
      if (conversationRef.current?.streamId === current.streamId) {
        setBusyAction(null);
      }
    }
  }, [client, projectConversation, reportActionError]);

  /** Remove one path-free staged upload from the next send request. */
  const handleRemoveAttachment = useCallback((uploadId: string): void => {
    setPendingAttachments((attachments) =>
      attachments.filter((attachment) => attachment.uploadId !== uploadId),
    );
  }, []);

  /** Emit a best-effort typing signal without surfacing ephemeral transport noise. */
  const handleTyping = useCallback((): void => {
    const current = conversationRef.current;
    if (current === null) {
      return;
    }
    void client
      .sendRoomTyping(current.roomId, current.streamId)
      .catch(() => undefined);
  }, [client]);

  /** Store explicit reader movement without coupling it to message arrival. */
  const handleLiveEdgeChange = useCallback(
    (atLiveEdge: boolean): void => {
      projectConversation((current) =>
        setConversationLiveEdge(current, atLiveEdge),
      );
    },
    [projectConversation],
  );

  /** Persist read-through only after the timeline proves the newest item visible. */
  const handleVisibleLatest = useCallback(
    (messageId: string): void => {
      const current = conversationRef.current;
      const latest = current?.messages[current.messages.length - 1];
      if (
        current === null ||
        latest?.id !== messageId ||
        current.unreadBoundary.kind === "none" ||
        readInFlightRef.current === messageId
      ) {
        return;
      }
      projectConversation((state) => setConversationLiveEdge(state, true));
      readInFlightRef.current = messageId;
      void client
        .markRoomRead(current.roomId, current.streamId, messageId)
        .then(() => {
          if (conversationRef.current?.streamId === current.streamId) {
            projectConversation((state) =>
              markConversationReadThrough(state, messageId),
            );
          }
        })
        .catch((error) => reportActionError(current.streamId, error))
        .finally(() => {
          if (readInFlightRef.current === messageId) {
            readInFlightRef.current = null;
          }
        });
    },
    [client, projectConversation, reportActionError],
  );

  if (openingError !== null) {
    return (
      <section className="room-conversation" aria-label="Room conversation">
        <div className="room-conversation__error" role="alert">
          <p>{openingError}</p>
          <button
            type="button"
            onClick={() => setAttempt((current) => current + 1)}
          >
            Retry room conversation
          </button>
        </div>
      </section>
    );
  }

  if (conversation === null) {
    return (
      <section className="room-conversation" aria-label="Room conversation">
        <p className="room-conversation__loading" role="status" aria-live="polite">
          Loading room conversation…
        </p>
      </section>
    );
  }

  const connectionAvailable = conversation.connectionStatus === "connected";
  return (
    <section className="room-conversation" aria-label="Room conversation">
      <p
        className={`room-conversation__connection room-conversation__connection--${conversation.connectionStatus}`}
        role="status"
        aria-label="Connection status"
        aria-live="polite"
        aria-atomic="true"
      >
        {connectionStatusLabel(conversation.connectionStatus)}
      </p>
      {actionError === null ? null : (
        <div className="room-conversation__action-error" role="alert">
          {actionError}
        </div>
      )}
      <MessageTimeline
        state={conversation}
        onLoadOlder={handleLoadOlder}
        onEditMessage={handleEdit}
        onDeleteMessage={handleDelete}
        onLiveEdgeChange={handleLiveEdgeChange}
        onVisibleLatest={handleVisibleLatest}
      />
      <RoomComposer
        key={conversation.streamId}
        attachments={pendingAttachments}
        progressByUploadId={conversation.uploadsByTransferId}
        canSend={conversation.permissions.sendMessages && connectionAvailable}
        canAttach={conversation.permissions.attachFiles && connectionAvailable}
        busy={busyAction !== null}
        onSelectAttachments={handleSelectAttachments}
        onRemoveAttachment={handleRemoveAttachment}
        onSend={handleSend}
        onTyping={handleTyping}
      />
    </section>
  );
}
