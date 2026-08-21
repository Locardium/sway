export function Switch({
  checked,
  onChange,
  disabled = false,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  /// Shown but not settable: the switch still reports its saved state, which
  /// is the point — a setting that another one currently overrides is not the
  /// same as one that is off.
  disabled?: boolean;
}) {
  return (
    <button
      role="switch"
      aria-checked={checked}
      aria-disabled={disabled}
      disabled={disabled}
      className={'switch' + (checked ? ' on' : '') + (disabled ? ' off-duty' : '')}
      onClick={() => onChange(!checked)}
    >
      <span className="switch-knob" />
    </button>
  );
}
