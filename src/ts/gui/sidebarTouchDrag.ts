import { htmlTouchDrag } from './htmlTouchDrag'

/** Touch input shares the sidebar's existing HTML drag/drop handlers. */
export function sidebarTouchDrag(node: HTMLElement) {
    return htmlTouchDrag(node, {
        source: '[data-sidebar-drag-id]',
        target: '[data-sidebar-drop]',
        ghost: '.avatar',
        identity: 'data-sidebar-drag-id',
    })
}
