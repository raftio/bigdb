"use client";
import * as React from "react";
import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/cn";

const button = cva(
  "inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded font-medium " +
    "transition-colors duration-fast disabled:pointer-events-none disabled:opacity-45 " +
    "[&_svg]:shrink-0 [&_svg]:size-3.5 select-none",
  {
    variants: {
      variant: {
        primary: "bg-accent text-accent-fg hover:bg-accent/88 shadow-e1",
        default: "bg-surface-raised text-fg border border-line hover:border-line-strong hover:bg-surface",
        ghost: "text-fg-muted hover:bg-surface-raised hover:text-fg",
        outline: "border border-line-strong text-fg hover:bg-surface-raised",
        danger: "bg-failed text-white hover:bg-failed/88",
        dangerOutline: "border border-failed/45 text-failed hover:bg-failed-soft",
        link: "text-accent underline-offset-2 hover:underline p-0 h-auto",
      },
      size: {
        xs: "h-6 px-2 text-xs",
        sm: "h-7 px-2.5 text-sm",
        md: "h-8 px-3 text-base",
        lg: "h-9 px-4 text-base",
        icon: "h-7 w-7",
        iconSm: "h-6 w-6",
      },
    },
    defaultVariants: { variant: "default", size: "sm" },
  },
);

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>, VariantProps<typeof button> {
  asChild?: boolean;
}

export const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild, ...props }, ref) => {
    const Comp = asChild ? Slot : "button";
    return <Comp ref={ref} className={cn(button({ variant, size }), className)} {...props} />;
  },
);
Button.displayName = "Button";
export { button as buttonVariants };
