import React from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { FluentProvider, webDarkTheme } from '@fluentui/react-components';
import { CsvInput, TransportBadge } from '../src/shared';

afterEach(cleanup);
const wrap = (child: React.ReactNode) => <FluentProvider theme={webDarkTheme}>{child}</FluentProvider>;

describe('truthful transport badge', () => {
  it('distinguishes live, explicit offline, and unknown modes', () => {
    const { rerender } = render(wrap(<TransportBadge mode="live" />));
    expect(screen.getByText('LIVE FOUNDRY TRANSPORT')).toBeTruthy();
    expect(screen.queryByText('OFFLINE TEST TRANSPORT')).toBeNull();
    rerender(wrap(<TransportBadge mode="offline" />));
    expect(screen.getByText('OFFLINE TEST TRANSPORT')).toBeTruthy();
    rerender(wrap(<TransportBadge mode={undefined} />));
    expect(screen.getByText('TRANSPORT UNKNOWN')).toBeTruthy();
    expect(screen.queryByText('LIVE FOUNDRY TRANSPORT')).toBeNull();
  });
});

describe('comma-separated configuration input', () => {
  it('retains a trailing separator while typing so a second key can be entered', () => {
    const onChange = vi.fn();
    const { rerender } = render(wrap(<CsvInput value={['first']} onChange={onChange} />));
    const input = screen.getByRole('textbox');
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: 'first,' } });
    rerender(wrap(<CsvInput value={['first']} onChange={onChange} />));
    expect((input as HTMLInputElement).value).toBe('first,');
    fireEvent.change(input, { target: { value: 'first, second' } });
    expect(onChange).toHaveBeenLastCalledWith(['first', 'second']);
  });
});
