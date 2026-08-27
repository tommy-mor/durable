import { test, expect } from '@playwright/test';
import { openTab, startHopd, stopHopd } from '../helpers';

test('microblog: follow then fan-out a post onto the follower timeline', async ({ browser }) => {
  const hopd = await startHopd('hoprt/hop/microblog.hop', 19120, 19121);
  try {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const a = await ctxA.newPage();
    const b = await ctxB.newPage();
    const sidA = await openTab(a, hopd.url);
    const sidB = await openTab(b, hopd.url);
    expect(sidA).toBeTruthy();
    expect(sidB).toBeTruthy();

    await a.locator('#name').fill('Ada');
    await a.locator('#join').click();
    await expect(b.locator('#users')).toContainText('Ada');

    await b.locator('#name').fill('Bob');
    await b.locator('#join').click();
    await expect(a.locator('#users')).toContainText('Bob');

    await b.locator(`#follow-${sidA}`).click();

    await a.locator('#draft').fill('hello tape');
    await a.locator('#post').click();

    await expect(a.locator('#feed li')).toContainText('hello tape');
    await expect(b.locator('#feed li')).toContainText('hello tape');

    // B posts; A did not follow B, so A's home feed must stay just Ada.
    // This also catches the "cast origin paints every tab" bug.
    await b.locator('#draft').fill('only bob');
    await b.locator('#post').click();
    await expect(b.locator('#feed li')).toHaveCount(2);
    await expect(b.locator('#feed li')).toContainText('only bob');
    await expect(a.locator('#feed li')).toHaveCount(1);
    await expect(a.locator('#feed li')).not.toContainText('only bob');

    await ctxA.close();
    await ctxB.close();
  } finally {
    await stopHopd(hopd);
  }
});
