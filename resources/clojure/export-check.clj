(ns clojurecraft.check
  "Compares values the way ClojureCraft checked them: like `=`, except that floating-point numbers
   may differ by a relative tolerance, numbers of different kinds (integer, float, BigDecimal,
   ratio) never match, instants (java.util.Date, java.time.Instant) match by their milliseconds,
   and with `:unordered` a sequential output is compared as a multiset.
   Written by ClojureCraft's project export."
  (:refer-clojure :exclude [time]))

(def default-opts {:float-tolerance 1e-9 :unordered false})

;; Readers for the #time/... tags of java.time values (see data_readers.clj).
(defn duration [s] (java.time.Duration/parse s))
(defn date [s] (java.time.LocalDate/parse s))
(defn date-time [s] (java.time.LocalDateTime/parse s))
(defn time [s] (java.time.LocalTime/parse s))

;; Read values are constants in the compiled tests, which the compiler embeds with print-dup.
(doseq [^Class c [java.time.Duration java.time.LocalDate java.time.LocalDateTime java.time.LocalTime]]
  (.addMethod ^clojure.lang.MultiFn print-dup c
              (fn [v ^java.io.Writer w]
                (.write w (str "#=(" (.getName c) "/parse \"" v "\")")))))

(defn- num-kind [x]
  (cond
    (or (instance? Double x) (instance? Float x)) :float
    (decimal? x) :decimal
    (rational? x) :rational
    :else :other))

(defn- floats-equal? [a b tolerance]
  (let [a (double a) b (double b)]
    (or (== a b)
        (and (Double/isNaN a) (Double/isNaN b))
        (and (not (Double/isInfinite a)) (not (Double/isInfinite b))
             (<= (Math/abs (- a b))
                 (max 1e-12 (* tolerance (max (Math/abs a) (Math/abs b)))))))))

(defn- at [path]
  (if (empty? path) "the output" (str "at " (pr-str path))))

(declare difference*)

(defn- seq-difference [e a path opts]
  (loop [i 0 e (seq e) a (seq a)]
    (cond
      (and (nil? e) (nil? a)) nil
      (nil? e) (str (at path) ": expected " i " elements but got more, starting with " (pr-str (first a)))
      (nil? a) (str (at path) ": expected " (+ i (count e)) " elements but got " i
                    "; missing " (pr-str (first e)))
      :else (or (difference* (first e) (first a) (conj path i) opts)
                (recur (inc i) (next e) (next a))))))

(defn- unordered-difference [e a path opts]
  (let [e (vec e) a (vec a)]
    (cond
      (= (frequencies e) (frequencies a)) nil
      (not= (count e) (count a))
      (str (at path) ": expected " (count e) " elements (in any order) but got " (count a))
      :else
      (loop [pending e remaining a]
        (if-let [x (first pending)]
          (let [i (first (keep-indexed (fn [i y] (when-not (difference* x y [] opts) i)) remaining))]
            (if (nil? i)
              (str (at path) ": no element matches the expected " (pr-str x) " (compared in any order)")
              (recur (rest pending) (into (subvec remaining 0 i) (subvec remaining (inc i))))))
          nil)))))

(defn- difference* [expected actual path opts]
  (cond
    (and (number? expected) (number? actual))
    (let [ke (num-kind expected) ka (num-kind actual)
          differs (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual))]
      (cond
        (not= ke ka) (str differs " (a different kind of number)")
        (= ke :float) (when-not (floats-equal? expected actual (:float-tolerance opts)) differs)
        (= ke :decimal) (when-not (zero? (.compareTo ^BigDecimal expected ^BigDecimal actual)) differs)
        :else (when-not (= expected actual) differs)))

    (and (map? expected) (map? actual))
    (let [missing (remove #(contains? actual %) (keys expected))
          extra (remove #(contains? expected %) (keys actual))]
      (cond
        (seq missing) (str (at path) ": missing key " (pr-str (first missing)))
        (seq extra) (str (at path) ": unexpected key " (pr-str (first extra)))
        :else (some (fn [[k v]] (difference* v (get actual k) (conj path k) opts)) expected)))

    (and (set? expected) (set? actual))
    (when-not (= expected actual)
      (let [missing (remove #(contains? actual %) expected)
            extra (remove #(contains? expected %) actual)]
        (str (at path) ": "
             (if (seq missing)
               (str "missing element " (pr-str (first missing)))
               (str "unexpected element " (pr-str (first extra)))))))

    (and (sequential? expected) (sequential? actual))
    (if (and (empty? path) (:unordered opts))
      (unordered-difference expected actual path opts)
      (seq-difference expected actual path opts))

    (and (inst? expected) (inst? actual))
    (when-not (= (inst-ms expected) (inst-ms actual))
      (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual)))

    (= expected actual) nil

    :else (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual))))

(defn difference
  "nil when `actual` matches `expected`, otherwise a description of the first difference."
  ([expected actual] (difference expected actual {}))
  ([expected actual opts] (difference* expected actual [] (merge default-opts opts))))

(defn throws?
  "Whether calling `f` (and realizing its result) throws an instance of the class named
  `class-name`, by simple or full name, a subclass included."
  [class-name f]
  (try
    (pr-str (f))
    false
    (catch Throwable t
      (loop [^Class c (class t)]
        (cond
          (nil? c) false
          (or (= class-name (.getName c)) (= class-name (.getSimpleName c))) true
          :else (recur (.getSuperclass c)))))))
