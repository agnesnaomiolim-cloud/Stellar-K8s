/**
 * Minimal type declarations for `d3-force-3d`, which ships without its
 * own typings and has no DefinitelyTyped package. The API mirrors
 * `d3-force` with an extra `dimension` parameter on the factories.
 */
declare module 'd3-force-3d' {
  export interface SimulationNodeDatum {
    x?: number;
    y?: number;
    z?: number;
    vx?: number;
    vy?: number;
    vz?: number;
    index?: number;
    fx?: number | null;
    fy?: number | null;
    fz?: number | null;
  }

  export interface SimulationLinkDatum<NodeT extends SimulationNodeDatum, LinkT = undefined> {
    source: string | number | NodeT;
    target: string | number | LinkT extends never ? NodeT : NodeT;
  }

  export interface Simulation<
    NodeT extends SimulationNodeDatum,
    LinkT extends SimulationLinkDatum<NodeT> | undefined = undefined,
  > {
    on(type: 'tick', listener: () => void): Simulation<NodeT, LinkT>;
    on(type: string, listener: (...args: unknown[]) => void): Simulation<NodeT, LinkT>;
    stop(): Simulation<NodeT, LinkT>;
    restart(): Simulation<NodeT, LinkT>;
    alpha(): number;
    alpha(value: number): Simulation<NodeT, LinkT>;
    alphaMin(value?: number): Simulation<NodeT, LinkT>;
    alphaDecay(value?: number): Simulation<NodeT, LinkT>;
    alphaTarget(value?: number): Simulation<NodeT, LinkT>;
    velocityDecay(value?: number): Simulation<NodeT, LinkT>;
    tick(): Simulation<NodeT, LinkT>;
    nodes(value?: NodeT[]): NodeT[] | Simulation<NodeT, LinkT>;
    force(name: string): unknown;
    force<F = unknown>(name: string, force: F): Simulation<NodeT, LinkT>;
  }

  export function forceSimulation<NodeT extends SimulationNodeDatum = SimulationNodeDatum>(
    nodes?: NodeT[],
    dimension?: number,
  ): Simulation<NodeT, never>;

  export interface ForceLink<NodeT extends SimulationNodeDatum, LinkT> {
    id(idFn: (node: NodeT, index: number, data: NodeT[]) => string): ForceLink<NodeT, LinkT>;
    distance(distance: number | ((link: LinkT, index: number, links: LinkT[]) => number)): ForceLink<NodeT, LinkT>;
    strength(strength: number | ((link: LinkT, index: number, links: LinkT[]) => number)): ForceLink<NodeT, LinkT>;
    links(links?: LinkT[]): LinkT[] | ForceLink<NodeT, LinkT>;
    iterations(count?: number): ForceLink<NodeT, LinkT>;
  }

  export function forceLink<NodeT extends SimulationNodeDatum = SimulationNodeDatum, LinkT = undefined>(
    links?: LinkT[],
    dimension?: number,
  ): ForceLink<NodeT, LinkT>;

  export interface ForceManyBody {
    strength(strength: number | ((node: SimulationNodeDatum, index: number, nodes: SimulationNodeDatum[]) => number)): ForceManyBody;
    distanceMax(distance?: number): number | ForceManyBody;
    distanceMin(distance?: number): number | ForceManyBody;
    theta(coeff?: number): number | ForceManyBody;
  }

  export function forceManyBody(dimension?: number): ForceManyBody;

  export interface ForceCenter {
    x(coord?: number): number | ForceCenter;
    y(coord?: number): number | ForceCenter;
    z(coord?: number): number | ForceCenter;
  }

  export function forceCenter(x?: number, y?: number, z?: number): ForceCenter;

  export interface ForceX {
    strength(strength: number | ((node: SimulationNodeDatum) => number)): ForceX;
    x(coord?: number): number | ForceX;
  }

  export function forceX(x?: number, dimension?: number): ForceX;

  export interface ForceY {
    strength(strength: number | ((node: SimulationNodeDatum) => number)): ForceY;
    y(coord?: number): number | ForceY;
  }

  export function forceY(y?: number, dimension?: number): ForceY;

  export interface ForceZ {
    strength(strength: number | ((node: SimulationNodeDatum) => number)): ForceZ;
    z(coord?: number): number | ForceZ;
  }

  export function forceZ(z?: number, dimension?: number): ForceZ;
}
