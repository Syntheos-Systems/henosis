/** Search and composable operational filters for the room directory. */
import {
  BellDot,
  CirclePause,
  ListTodo,
  MailOpen,
  Search,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import type { RoomFilter } from "../domain/rooms";

/** Metadata for one visible room filter control. */
interface FilterOption {
  /** Stable domain filter identifier. */
  id: RoomFilter;
  /** Human-readable control text. */
  label: string;
  /** Visual signifier paired with the text label. */
  icon: LucideIcon;
}

/** Product-approved room filter controls. */
const FILTER_OPTIONS: FilterOption[] = [
  { id: "unread", label: "Unread", icon: MailOpen },
  { id: "active-work", label: "Active work", icon: ListTodo },
  { id: "approvals", label: "Approvals", icon: BellDot },
  { id: "paused", label: "Paused", icon: CirclePause },
];

/** Inputs and callbacks for controlled room search and filter state. */
export interface RoomFiltersProps {
  /** Current text query. */
  query: string;
  /** Current active operational filters. */
  filters: ReadonlySet<RoomFilter>;
  /** Update the text query. */
  onQueryChange(query: string): void;
  /** Toggle one operational filter. */
  onFilterToggle(filter: RoomFilter): void;
}

/** Render keyboard-accessible search and status filters. */
export function RoomFilters({
  query,
  filters,
  onQueryChange,
  onFilterToggle,
}: RoomFiltersProps) {
  return (
    <div className="room-tools">
      <div className="room-search">
        <Search aria-hidden="true" />
        <label className="sr-only" htmlFor="room-search">
          Search rooms, servers, messages, and participants
        </label>
        <input
          id="room-search"
          type="search"
          value={query}
          onChange={(event) => onQueryChange(event.target.value)}
          placeholder="Search rooms, people, or messages"
        />
        <kbd aria-label="Keyboard shortcut">⌘ K</kbd>
      </div>

      <div className="filter-list" aria-label="Filter rooms">
        {FILTER_OPTIONS.map((option) => {
          const Icon = option.icon;
          const selected = filters.has(option.id);
          return (
            <button
              className="filter-chip"
              data-active={selected}
              type="button"
              aria-pressed={selected}
              key={option.id}
              onClick={() => onFilterToggle(option.id)}
            >
              <Icon aria-hidden="true" />
              {option.label}
            </button>
          );
        })}
      </div>
    </div>
  );
}
