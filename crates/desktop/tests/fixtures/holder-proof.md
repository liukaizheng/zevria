## Hölder’s inequality

Let \(p,q>1\) satisfy
\[
\frac1p+\frac1q=1.
\]
For any real or complex numbers \(a_1,\ldots,a_n\) and \(b_1,\ldots,b_n\), Hölder’s inequality states that
\[
\boxed{\displaystyle
\sum_{i=1}^{n}|a_i b_i|
\le
\left(\sum_{i=1}^{n}|a_i|^{p}\right)^{1/p}
\left(\sum_{i=1}^{n}|b_i|^{q}\right)^{1/q}.
}
\]
We prove it using Young’s inequality and normalization. ([math.mit.edu](https://math.mit.edu/~rbm/18-102-S14/Chapter5.pdf))

### 1. Prove Young’s inequality

For \(u,v\ge0\), Young’s inequality is
\[
uv\le \frac{u^p}{p}+\frac{v^q}{q}.
\]
([maths.tcd.ie](https://www.maths.tcd.ie/~dwilkins/Courses/MAU22200/MAU22200_Hil2020_CourseNotes/MAU22200_Hil2020_PartIV.pdf))

To prove this, fix \(v\ge0\) and consider
\[
F(u)=\frac{u^p}{p}-uv+\frac{v^q}{q},
\qquad u\ge0.
\]
Since
\[
F'(u)=u^{p-1}-v,
\]
the function attains its minimum at
\[
u=v^{1/(p-1)}=v^{q-1}.
\]
Using \(p(q-1)=q\), its minimum value is
\[
F\!\left(v^{q-1}\right)
=\frac{v^q}{p}-v^q+\frac{v^q}{q}
=v^q\left(\frac1p+\frac1q-1\right)=0.
\]
Thus \(F(u)\ge0\), which proves Young’s inequality. ([math.mit.edu](https://math.mit.edu/~rbm/18-102-S14/Chapter5.pdf))

### 2. Normalize and apply Young’s inequality

Set
\[
A=\left(\sum_{i=1}^{n}|a_i|^p\right)^{1/p},
\qquad
B=\left(\sum_{i=1}^{n}|b_i|^q\right)^{1/q}.
\]
If \(A=0\) or \(B=0\), every product \(a_i b_i\) is zero, so the desired inequality holds.

Otherwise, apply Young’s inequality with
\[
u=\frac{|a_i|}{A},
\qquad
v=\frac{|b_i|}{B}.
\]
Summing over \(i\) gives
\[
\begin{aligned}
\frac{1}{AB}\sum_{i=1}^{n}|a_i b_i|
&\le
\frac1p\frac{\sum_{i=1}^{n}|a_i|^p}{A^p}
+
\frac1q\frac{\sum_{i=1}^{n}|b_i|^q}{B^q}\\[2mm]
&=\frac1p+\frac1q\\
&=1.
\end{aligned}
\]
Multiplying by \(AB\) proves Hölder’s inequality. \(\square\) ([math.mit.edu](https://math.mit.edu/~rbm/18-102-S14/Chapter5.pdf))

### Integral form

The same proof, replacing sums by integrals, yields
\[
\boxed{\displaystyle
\int_X |fg|\,d\mu
\le
\left(\int_X |f|^p\,d\mu\right)^{1/p}
\left(\int_X |g|^q\,d\mu\right)^{1/q}
}
\]
whenever \(f\in L^p(\mu)\) and \(g\in L^q(\mu)\). ([maths.tcd.ie](https://www.maths.tcd.ie/~dwilkins/Courses/MAU22200/MAU22200_Hil2020_CourseNotes/MAU22200_Hil2020_PartIV.pdf))