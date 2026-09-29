/**
 * Tree Layout — computes 2D positions for an auth tree's nodes so the canvas
 * can render them (and the SVG lines connecting them) at arbitrary depth.
 *
 * Pure and framework-agnostic: takes the domain tree from `auth_validator`
 * and returns plain position/edge data, no DOM or React involved.
 */

import type { AuthTreeNode } from '../../../utils/auth_validator';
import type { LayoutPosition } from './types';

export const COL_WIDTH = 260;
export const ROW_HEIGHT = 96;

export interface TreeEdge {
  parentId: string;
  childId: string;
}

interface LayoutResult {
  positions: LayoutPosition[];
  edges: TreeEdge[];
}

function layoutSubtree(
  node: AuthTreeNode,
  depth: number,
  counter: { next: number },
  positions: LayoutPosition[],
  edges: TreeEdge[],
): number {
  let y: number;

  if (node.children.length === 0) {
    y = counter.next * ROW_HEIGHT;
    counter.next += 1;
  } else {
    const childYs = node.children.map((child) => {
      edges.push({ parentId: node.id, childId: child.id });
      return layoutSubtree(child, depth + 1, counter, positions, edges);
    });
    y = (Math.min(...childYs) + Math.max(...childYs)) / 2;
  }

  positions.push({ id: node.id, depth, x: depth * COL_WIDTH, y });
  return y;
}

/** Computes positions + connecting edges for a forest of address-rooted trees. */
export function layoutForest(roots: AuthTreeNode[]): LayoutResult {
  const positions: LayoutPosition[] = [];
  const edges: TreeEdge[] = [];
  const counter = { next: 0 };

  for (const root of roots) {
    layoutSubtree(root, 0, counter, positions, edges);
  }

  return { positions, edges };
}
