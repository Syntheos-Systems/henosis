/** Chronological room timeline with explicit history and live-edge controls. */
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { UIEvent } from "react";
import type { ConversationState } from "../domain/conversation";
import { MessageItem } from "./MessageItem";

/** Maximum distance from the bottom still treated as visibly live. */
const LIVE_EDGE_THRESHOLD_PX = 32;

/** Scroll position captured before an older page is prepended. */
interface ScrollAnchor {
  /** Scroll height before the history request. */
  height: number;
  /** Viewport offset before the history request. */
  top: number;
  /** Shared sequence that must project before layout restoration. */
  throughSequence: number | null;
}

/** Props accepted by the chronological message timeline. */
interface MessageTimelineProps {
  /** Current immutable room conversation projection. */
  state: ConversationState;
  /** Load the next older page and return its shared sequence on success. */
  onLoadOlder(): Promise<number | null>;
  /** Persist an owned message edit. */
  onEditMessage(messageId: string, content: string): Promise<boolean>;
  /** Persist an authorized message deletion. */
  onDeleteMessage(messageId: string): Promise<boolean>;
  /** Replace the reducer's explicit live-edge state. */
  onLiveEdgeChange(atLiveEdge: boolean): void;
  /** Report proof that the newest message is visibly at the live edge. */
  onVisibleLatest(messageId: string): void;
}

/** Return true only when the newest edge is within the visible viewport. */
function isAtVisibleLiveEdge(element: HTMLElement): boolean {
  return (
    element.scrollHeight - element.scrollTop - element.clientHeight <=
    LIVE_EDGE_THRESHOLD_PX
  );
}

/** Render the first-unread structural separator. */
function UnreadDivider() {
  return (
    <div
      className="message-timeline__unread"
      role="separator"
      aria-label="Unread messages"
    >
      <span>Unread messages</span>
    </div>
  );
}

/** Render chronological messages while preserving reader position and intent. */
export function MessageTimeline({
  state,
  onLoadOlder,
  onEditMessage,
  onDeleteMessage,
  onLiveEdgeChange,
  onVisibleLatest,
}: MessageTimelineProps) {
  const timelineRef = useRef<HTMLDivElement>(null);
  const liveSentinelRef = useRef<HTMLDivElement>(null);
  const scrollAnchorRef = useRef<ScrollAnchor | null>(null);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [anchorRevision, setAnchorRevision] = useState(0);
  const latestMessage = state.messages[state.messages.length - 1];
  const activeTyping = [...state.typingByUserId.values()].filter(
    (indicator) => indicator.userId !== state.currentUserId,
  );

  /** Restore the same visible content after a successful history prepend. */
  useLayoutEffect(() => {
    const timeline = timelineRef.current;
    const anchor = scrollAnchorRef.current;
    if (
      timeline === null ||
      anchor === null ||
      anchor.throughSequence === null ||
      state.lastEventSequence < anchor.throughSequence
    ) {
      return;
    }
    timeline.scrollTop = anchor.top + (timeline.scrollHeight - anchor.height);
    scrollAnchorRef.current = null;
  }, [anchorRevision, state.lastEventSequence, state.messages]);

  /** Observe the bottom sentinel so read marking follows actual visibility. */
  useEffect(() => {
    const timeline = timelineRef.current;
    const sentinel = liveSentinelRef.current;
    if (
      timeline === null ||
      sentinel === null ||
      latestMessage === undefined ||
      typeof IntersectionObserver === "undefined"
    ) {
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries.some((entry) => entry.isIntersecting);
        onLiveEdgeChange(visible);
        if (visible) {
          onVisibleLatest(latestMessage.id);
        }
      },
      { root: timeline, threshold: 1 },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [latestMessage?.id, onLiveEdgeChange, onVisibleLatest]);

  /** Load older messages and arm layout restoration after the parent projects them. */
  async function loadOlder(): Promise<void> {
    const timeline = timelineRef.current;
    if (timeline === null || loadingOlder) {
      return;
    }
    const anchor: ScrollAnchor = {
      height: timeline.scrollHeight,
      top: timeline.scrollTop,
      throughSequence: null,
    };
    scrollAnchorRef.current = anchor;
    setLoadingOlder(true);
    const throughSequence = await onLoadOlder();
    setLoadingOlder(false);
    if (throughSequence === null) {
      if (scrollAnchorRef.current === anchor) {
        scrollAnchorRef.current = null;
      }
      return;
    }
    anchor.throughSequence = throughSequence;
    setAnchorRevision((revision) => revision + 1);
  }

  /** Update explicit live-edge state from a direct reader scroll. */
  function handleScroll(event: UIEvent<HTMLDivElement>): void {
    const visible = isAtVisibleLiveEdge(event.currentTarget);
    onLiveEdgeChange(visible);
    if (visible && latestMessage !== undefined) {
      onVisibleLatest(latestMessage.id);
    }
  }

  /** Move toward the newest message while visibility observers prove arrival. */
  function jumpToLatest(): void {
    const timeline = timelineRef.current;
    if (timeline === null || latestMessage === undefined) {
      return;
    }
    const reduceMotion =
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (typeof timeline.scrollTo === "function") {
      timeline.scrollTo({
        top: timeline.scrollHeight,
        behavior: reduceMotion ? "auto" : "smooth",
      });
    } else {
      timeline.scrollTop = timeline.scrollHeight;
    }
  }

  /** Delete a message and retain a useful focus target when its item disappears. */
  async function deleteMessage(messageId: string): Promise<boolean> {
    const deleted = await onDeleteMessage(messageId);
    if (deleted) {
      timelineRef.current?.focus();
    }
    return deleted;
  }

  /** Build the concise typing announcement for one or more participants. */
  function typingAnnouncement(): string {
    const usernames = activeTyping.map((indicator) => indicator.username);
    if (usernames.length === 0) {
      return "";
    }
    if (usernames.length === 1) {
      return `${usernames[0]} is typing…`;
    }
    return `${usernames.join(", ")} are typing…`;
  }

  return (
    <section className="message-timeline" aria-label="Room messages">
      {state.hasOlder ? (
        <button
          className="message-timeline__load-older"
          type="button"
          disabled={loadingOlder}
          onClick={() => void loadOlder()}
        >
          {loadingOlder ? "Loading earlier messages…" : "Load earlier messages"}
        </button>
      ) : null}

      <div
        ref={timelineRef}
        className="message-timeline__scroll"
        role="log"
        aria-label="Room message timeline"
        aria-live="polite"
        tabIndex={-1}
        onScroll={handleScroll}
      >
        {state.unreadBoundary.kind === "beforeLoadedWindow" ? (
          <UnreadDivider />
        ) : null}
        {state.messages.length === 0 ? (
          <p className="message-timeline__empty">No messages in this room yet.</p>
        ) : (
          state.messages.map((message) => (
            <div className="message-timeline__entry" key={message.id}>
              {state.unreadBoundary.kind === "beforeMessage" &&
              state.unreadBoundary.messageId === message.id ? (
                <UnreadDivider />
              ) : null}
              <MessageItem
                message={message}
                currentUserId={state.currentUserId}
                permissions={state.permissions}
                presence={state.presenceByUserId.get(message.authorId)}
                onEdit={onEditMessage}
                onDelete={deleteMessage}
              />
            </div>
          ))
        )}
        <div
          ref={liveSentinelRef}
          className="message-timeline__live-sentinel"
          aria-hidden="true"
        />
      </div>

      {state.atLiveEdge ? null : (
        <button
          className="message-timeline__jump-latest"
          type="button"
          onClick={jumpToLatest}
        >
          Jump to latest
        </button>
      )}
      <p className="message-timeline__typing" role="status" aria-live="polite">
        {typingAnnouncement()}
      </p>
    </section>
  );
}
