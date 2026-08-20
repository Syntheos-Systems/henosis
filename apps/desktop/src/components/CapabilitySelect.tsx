/** Native accessible select for deployment-discovered capabilities. */

/** One deployment-supplied option rendered without a frontend allowlist. */
export interface CapabilitySelectOption {
  /** Stable value submitted to the room agent reducer. */
  readonly id: string;
  /** Human-facing deployment label. */
  readonly label: string;
  /** Whether the connected deployment can currently use this choice. */
  readonly available: boolean;
  /** Safe deployment-supplied reason when the choice is unavailable. */
  readonly unavailableReason?: string | null;
}

/** Inputs for one labeled capability selector. */
export interface CapabilitySelectProps {
  /** Stable DOM identifier joining the label and control. */
  readonly id: string;
  /** Visible field label. */
  readonly label: string;
  /** Agent-specific accessible name. */
  readonly ariaLabel: string;
  /** Current stable capability identifier. */
  readonly value: string;
  /** Deployment-discovered choices in display order. */
  readonly options: readonly CapabilitySelectOption[];
  /** Placeholder shown when no valid choice is selected. */
  readonly placeholder: string;
  /** Whether ownership or room policy prevents editing. */
  readonly disabled: boolean;
  /** Receive one stable available capability identifier. */
  readonly onChange: (value: string) => void;
}

/** Render a keyboard-native capability selector with unavailable choices explained. */
export function CapabilitySelect({
  id,
  label,
  ariaLabel,
  value,
  options,
  placeholder,
  disabled,
  onChange,
}: CapabilitySelectProps) {
  return (
    <label className="agent-field" htmlFor={id}>
      <span>{label}</span>
      <select
        id={id}
        aria-label={ariaLabel}
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.currentTarget.value)}
      >
        <option value="" disabled>
          {placeholder}
        </option>
        {options.map((option) => (
          <option
            key={option.id}
            value={option.id}
            disabled={!option.available}
            title={option.unavailableReason ?? undefined}
          >
            {option.available ? option.label : `${option.label} · unavailable`}
          </option>
        ))}
      </select>
    </label>
  );
}
