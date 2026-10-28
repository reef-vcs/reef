namespace Geometry {
  export interface Shape {
    area(): number;
  }
}

export enum Color {
  Red,
  Green,
}

type Point = { x: number; y: number };

abstract class Base implements Geometry.Shape {
  abstract area(): number;
}

export class Circle extends Base {
  radius: number = 1;

  constructor(radius: number) {
    super();
    this.radius = radius;
  }

  area(): number {
    return Math.PI * square(this.radius);
  }
}

function square(n: number): number {
  return n * n;
}

export const makeCircle = (r: number): Circle => new Circle(r);
