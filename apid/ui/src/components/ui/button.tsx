import { Button as BaseButton } from '@base-ui/react/button'
import { cva, type VariantProps } from 'class-variance-authority'
import type { ComponentProps } from 'react'
import { cn } from '@/lib/utils'

const buttonVariants = cva(
  'inline-flex min-h-11 items-center justify-center gap-2 rounded-full px-4 text-sm font-semibold transition-colors focus-visible:outline-none focus-visible:ring-3 focus-visible:ring-ring/40 disabled:pointer-events-none disabled:opacity-45',
  {
    variants: {
      variant: {
        primary: 'bg-accent text-accent-foreground shadow-sm hover:bg-accent/88',
        secondary: 'border border-input bg-surface text-foreground hover:bg-muted',
        danger: 'bg-danger text-white hover:bg-danger/88',
        ghost: 'text-muted-foreground hover:bg-muted hover:text-foreground',
      },
      size: { default: 'h-11', sm: 'h-11 px-3', lg: 'h-12 px-5 text-base' },
    },
    defaultVariants: { variant: 'primary', size: 'default' },
  },
)

export function Button({
  className,
  variant,
  size,
  ...props
}: ComponentProps<typeof BaseButton> & VariantProps<typeof buttonVariants>) {
  return <BaseButton className={cn(buttonVariants({ variant, size }), className)} {...props} />
}
