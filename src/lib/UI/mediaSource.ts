import type { Action } from 'svelte/action'

export const loadMediaSource: Action<HTMLSourceElement, string | null | undefined> = (
  node,
  url,
) => {
  const update = (nextUrl: string | null | undefined) => {
    const normalizedUrl = nextUrl ?? ''
    if (normalizedUrl === (node.getAttribute('src') ?? '')) return

    // A child source change does not restart media selection, so keep the attribute and load() together.
    if (normalizedUrl) node.setAttribute('src', normalizedUrl)
    else node.removeAttribute('src')

    if (node.parentElement instanceof HTMLMediaElement) node.parentElement.load()
  }

  update(url)
  return { update }
}
